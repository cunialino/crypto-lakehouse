//! The Iceberg sink: turn one micro-batch into parquet data files and commit them as a
//! single `fast_append` snapshot.
//!
//! Shape of one commit:
//!
//! ```text
//! rows ──► RecordBatch ──► RecordBatchPartitionSplitter (identity exchange, day trade_ts)
//!        ──► FanoutWriter(DataFileWriter(RollingFileWriter(ParquetWriter)))  [1 writer/partition]
//!        ──► Vec<DataFile> ──► Transaction::fast_append().commit(catalog)
//! ```
//!
//! One micro-batch == one snapshot == the atomic unit of visibility, which is also what
//! makes the pipeline's "ack after commit" safe.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use arrow_array::{Array, RecordBatch, StringArray, TimestampMicrosecondArray};
use datafusion::arrow::compute::concat_batches;
use datafusion::catalog::CatalogProvider;
use datafusion::datasource::TableProvider;
use datafusion::execution::context::SessionContext;
use datafusion::logical_expr::Expr;
use futures::{TryFutureExt, TryStreamExt};
use iceberg::io::{
    S3_ACCESS_KEY_ID, S3_ENDPOINT, S3_PATH_STYLE_ACCESS, S3_REGION, S3_SECRET_ACCESS_KEY,
};
use iceberg::spec::DataFileFormat;
use iceberg::table::Table;
use iceberg::transaction::{ApplyTransactionAction, Transaction};
use iceberg::writer::base_writer::data_file_writer::DataFileWriterBuilder;
use iceberg::writer::file_writer::ParquetWriterBuilder;
use iceberg::writer::file_writer::location_generator::{
    DefaultFileNameGenerator, DefaultLocationGenerator,
};
use iceberg::writer::file_writer::rolling_writer::RollingFileWriterBuilder;
use iceberg::writer::partitioning::PartitioningWriter;
use iceberg::writer::partitioning::fanout_writer::FanoutWriter;
use iceberg::{Catalog, CatalogBuilder, NamespaceIdent, TableCreation, TableIdent};
use iceberg_catalog_rest::{
    REST_CATALOG_PROP_URI, REST_CATALOG_PROP_WAREHOUSE, RestCatalogBuilder,
};
use iceberg_datafusion::IcebergCatalogProvider;
use iceberg_datafusion::physical_plan::convert_filters_to_predicate;
use iceberg_storage_opendal::OpenDalStorageFactory;
use parquet::basic::{Compression as ParquetCompression, ZstdLevel};
use parquet::file::properties::{WriterProperties, WriterVersion};
use uuid::Uuid;

use crate::config::IcebergConfig;
use crate::schema::{self, TradeEvent};

/// What one flush produced; logged and returned to the caller for stats.
#[derive(Debug, Default, Clone, Copy)]
pub struct WriteStats {
    pub rows: usize,
    pub files: usize,
    pub partitions: usize,
    pub bytes: u64,
    pub write_elapsed: Duration,
    pub commit_elapsed: Duration,
    pub probe: ProbeStats,
}

/// What one dedup probe read, so a flush log line can show whether the pruning expression
/// is still doing its job: `files_opened` tracks the `(exchange, day)` spread of the
/// micro-batch, while `data_files` only ever grows.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ProbeStats {
    pub files_opened: usize,
    pub partitions_opened: usize,
    pub bytes_opened: u64,
    /// Data files in the snapshot the probe read; `0` for a table with no snapshot yet.
    pub data_files: usize,
}

pub struct IcebergSink {
    catalog: Arc<dyn Catalog>,
    ident: TableIdent,
    table: Table,
    /// Process-wide so the embedded file counter never restarts and file names stay unique
    /// across flushes; the uuid keeps them unique across restarts.
    file_name_gen: DefaultFileNameGenerator,
    cfg: IcebergConfig,
    ctx: SessionContext,
}

fn build_pruning_expr(batch: &RecordBatch, schema: &arrow_schema::Schema) -> Result<Expr> {
    let ts_dt = schema
        .field_with_name("trade_ts")
        .context("iceberg schema has no `trade_ts`")?
        .data_type()
        .clone();
    let ts = col_as::<TimestampMicrosecondArray>(batch, "trade_ts")?;
    let ts_lit = |v: i64| -> Result<Expr> {
        let sv = match &ts_dt {
            arrow_schema::DataType::Timestamp(arrow_schema::TimeUnit::Microsecond, tz) => {
                datafusion::scalar::ScalarValue::TimestampMicrosecond(Some(v), tz.clone())
            }
            arrow_schema::DataType::Timestamp(arrow_schema::TimeUnit::Nanosecond, tz) => {
                datafusion::scalar::ScalarValue::TimestampNanosecond(
                    Some(v.checked_mul(1_000).context("ns overflow")?),
                    tz.clone(),
                )
            }
            arrow_schema::DataType::Timestamp(arrow_schema::TimeUnit::Millisecond, tz) => {
                datafusion::scalar::ScalarValue::TimestampMillisecond(
                    Some(v.div_euclid(1_000)),
                    tz.clone(),
                )
            }
            arrow_schema::DataType::Timestamp(arrow_schema::TimeUnit::Second, tz) => {
                datafusion::scalar::ScalarValue::TimestampSecond(
                    Some(v.div_euclid(1_000_000)),
                    tz.clone(),
                )
            }
            other => panic!("iceberg `trade_ts` is {other:?}, expected a timestamp"),
        };
        Ok(datafusion::logical_expr::lit(sv))
    };

    let ex = col_as::<StringArray>(batch, "exchange")?;
    let sym = col_as::<StringArray>(batch, "symbol")?;
    const MICROS_PER_DAY: i64 = 86_400_000_000;

    let mut groups: HashMap<(&str, i64), (i64, i64)> = HashMap::new(); // (exchange, day) → ts bounds
    let mut syms: HashSet<&str> = HashSet::new();

    for i in 0..batch.num_rows() {
        if ts.is_null(i) || ex.is_null(i) || sym.is_null(i) {
            continue;
        }
        let t = ts.value(i);
        let g = groups
            .entry((ex.value(i), t.div_euclid(MICROS_PER_DAY)))
            .or_insert((i64::MAX, i64::MIN));
        g.0 = g.0.min(t);
        g.1 = g.1.max(t);
        syms.insert(sym.value(i));
    }

    let pruning = groups
        .iter()
        .map(|((e, _day), (lo, hi))| -> Result<Expr> {
            let ll = ts_lit(*lo)?;
            let hh = ts_lit(*hi)?;
            Ok(datafusion::logical_expr::col("exchange")
                .eq(datafusion::logical_expr::lit(*e))
                .and(datafusion::logical_expr::col("trade_ts").gt_eq(ll))
                .and(datafusion::logical_expr::col("trade_ts").lt_eq(hh)))
        })
        .collect::<Result<Vec<Expr>, _>>()? // ← ts_lit can fail; fold can't carry errors
        .into_iter()
        // `reduce`, not `fold(lit(false))`: iceberg's translator rejects any OR that has a
        // literal side, so a `false` seed would silently cost the probe its pushdown.
        // `probe_stats` converts this same expression, and the flush test asserts both
        // sides agree.
        .reduce(Expr::or)
        .unwrap_or_else(|| datafusion::logical_expr::lit(false));

    Ok(pruning)
}

fn col_as<'a, A: Array + 'static>(batch: &'a RecordBatch, name: &str) -> anyhow::Result<&'a A> {
    let arr = batch
        .column_by_name(name)
        .ok_or_else(|| anyhow::anyhow!("missing column `{name}`"))?;

    let typed = arr
        .as_any()
        .downcast_ref::<A>()
        .ok_or_else(|| anyhow::anyhow!("column `{name}` not a {}", std::any::type_name::<A>()))?;

    Ok(typed)
}

impl IcebergSink {
    /// Connects to the Lakekeeper REST catalog and makes sure the destination table exists.
    pub async fn connect(cfg: IcebergConfig) -> Result<Self> {
        let catalog: Arc<dyn Catalog> = Arc::new(
            RestCatalogBuilder::default()
                .with_storage_factory(Arc::new(OpenDalStorageFactory::S3 {
                    customized_credential_load: None,
                }))
                .load("lakekeeper", catalog_props(&cfg))
                .await
                .context("failed to load lakekeeper REST catalog")?,
        );
        Self::with_catalog(catalog, cfg).await
    }

    /// Same as [`Self::connect`] but against an already-built catalog (tests use the
    /// in-memory catalog; nothing else in the sink is REST-specific).
    pub async fn with_catalog(catalog: Arc<dyn Catalog>, cfg: IcebergConfig) -> Result<Self> {
        let ident = TableIdent::new(
            NamespaceIdent::new(cfg.namespace.clone()),
            cfg.table.clone(),
        );
        let table = ensure_table(&catalog, &ident, &cfg).await?;

        tracing::info!(
            table = %ident,
            location = table.metadata().location(),
            snapshot = ?table.metadata().current_snapshot().map(|s| s.snapshot_id()),
            "iceberg sink ready"
        );

        let ctx = SessionContext::new();
        ctx.register_table("icetbl", probe_view_provider(&catalog, &cfg).await?)?;

        Ok(Self {
            catalog,
            ident,
            table,
            file_name_gen: DefaultFileNameGenerator::new(
                format!("batcher-{}", Uuid::now_v7().simple()),
                None,
                DataFileFormat::Parquet,
            ),
            cfg,
            ctx,
        })
    }

    pub fn table(&self) -> &Table {
        &self.table
    }

    pub async fn reload(&mut self) -> Result<()> {
        self.table = self
            .catalog
            .load_table(&self.ident)
            .await
            .with_context(|| format!("failed to reload table {}", self.ident))?;
        Ok(())
    }

    /// The probe side of the dedup anti-join: committed rows restricted to the
    /// `(exchange, trade_ts)` ranges of `incoming`, plus what that restriction costs.
    ///
    /// The restriction is what keeps the probe cheap — DataFusion hands the predicate to
    /// the Iceberg scan, which drops whole data files (partition + column-range metrics)
    /// before reading them. Tests assert exactly that, see the `probe_*` tests below.
    async fn dedup_probe(
        &self,
        incoming: &RecordBatch,
        schema: &arrow_schema::Schema,
    ) -> Result<(datafusion::dataframe::DataFrame, ProbeStats)> {
        let pruning = build_pruning_expr(incoming, schema)?;
        let probe = self.probe_stats(&pruning).await?;
        Ok((self.ctx.table("icetbl").await?.filter(pruning)?, probe))
    }

    async fn deduped_batch(
        &self,
        incoming: &RecordBatch,
        schema: &arrow_schema::Schema,
    ) -> Result<(Vec<RecordBatch>, ProbeStats)> {
        self.ctx.register_batch("incoming", incoming.clone())?;

        let (cand, probe) = self.dedup_probe(incoming, schema).await?;

        let out = self
            .ctx
            .table("incoming")
            .await?
            .join(
                cand,
                datafusion::common::JoinType::LeftAnti,
                &["exchange", "symbol", "trade_id"],
                &["exchange", "symbol", "trade_id"],
                None,
            )?
            .collect()
            .await?;

        self.ctx.deregister_table("incoming")?;
        Ok((out, probe))
    }

    /// How much of the table the probe's predicate opens, planned with the very predicate
    /// the scan is handed and against the snapshot that scan will load — the two halves of
    /// a `probe_files=N of M` log line are only comparable if they name the same snapshot.
    ///
    /// Costs one metadata load plus one manifest planning pass per micro-batch; no data
    /// file is read twice, since this only walks manifests.
    async fn probe_stats(&self, pruning: &Expr) -> Result<ProbeStats> {
        let table = self
            .catalog
            .load_table(&self.ident)
            .await
            .context("failed to load table for probe statistics")?;

        let mut scan = table.scan().select_all();
        if let Some(predicate) = convert_filters_to_predicate(std::slice::from_ref(pruning)) {
            scan = scan.with_filter(predicate);
        }
        let tasks = scan
            .build()?
            .plan_files()
            .await?
            .try_collect::<Vec<_>>()
            .await?;

        Ok(ProbeStats {
            files_opened: tasks.len(),
            partitions_opened: tasks
                .iter()
                .map(|task| partition_dirs(task.data_file_path()))
                .collect::<HashSet<_>>()
                .len(),
            bytes_opened: tasks.iter().map(|task| task.file_size_in_bytes).sum(),
            data_files: total_data_files(&table),
        })
    }

    /// Writes all rows of one micro-batch and commits them as one snapshot.
    ///
    /// Commit failures are retried with exponential backoff (the table metadata is
    /// reloaded between attempts, so a concurrent committer only costs a retry). On
    /// *final* failure the error is returned and the caller must NOT ack the source
    /// messages, which turns the failure into JetStream redelivery instead of data loss.
    /// Data files written by a failed attempt become orphaned objects; that is the normal
    /// Iceberg trade-off and is reclaimed by table maintenance (`remove_orphan_files`).
    pub async fn write_batch(&mut self, rows: &[TradeEvent]) -> Result<WriteStats> {
        if rows.is_empty() {
            return Ok(WriteStats::default());
        }

        let write_start = Instant::now();
        let (data_files, probe) = self.write_files(rows).await?;
        let write_elapsed = write_start.elapsed();

        let file_count = data_files.len();
        let partitions = data_files
            .iter()
            .map(|f| f.partition().clone())
            .collect::<std::collections::HashSet<_>>()
            .len();
        let bytes = data_files.iter().map(|f| f.file_size_in_bytes()).sum();

        let commit_start = Instant::now();
        self.commit(data_files).await?;
        let commit_elapsed = commit_start.elapsed();

        Ok(WriteStats {
            rows: rows.len(),
            files: file_count,
            partitions,
            bytes,
            write_elapsed,
            commit_elapsed,
            probe,
        })
    }

    async fn write_files(
        &self,
        rows: &[TradeEvent],
    ) -> Result<(Vec<iceberg::spec::DataFile>, ProbeStats)> {
        let metadata = self.table.metadata();
        let iceberg_schema = metadata.current_schema().clone();
        let arrow_schema = schema::arrow_schema(&iceberg_schema)?;
        let partition_spec = metadata.default_partition_spec().clone();

        let batch = schema::to_record_batch(&arrow_schema, rows)?;
        let (deduped_batches, probe) = self.deduped_batch(&batch, &arrow_schema).await?;
        let batched_dep = concat_batches(&arrow_schema, &deduped_batches)?;
        let splitter = iceberg::arrow::RecordBatchPartitionSplitter::try_new_with_computed_values(
            iceberg_schema.clone(),
            partition_spec.clone(),
        )
        .context("failed to build partition splitter")?;
        let partitions = splitter
            .split(&batched_dep)
            .context("failed to split micro-batch by partition")?;

        let parquet_builder = ParquetWriterBuilder::new(
            writer_properties(&self.cfg.parquet_compression),
            iceberg_schema.clone(),
        );
        let rolling = RollingFileWriterBuilder::new(
            parquet_builder,
            self.cfg.target_file_size,
            self.table.file_io().clone(),
            DefaultLocationGenerator::new(metadata)
                .context("failed to derive data location from table metadata")?,
            self.file_name_gen.clone(),
        );

        let mut writer = FanoutWriter::new(DataFileWriterBuilder::new(rolling));
        for (partition_key, part) in partitions {
            writer
                .write(partition_key, part)
                .await
                .context("failed to write partition rows")?;
        }
        Ok((
            writer
                .close()
                .await
                .context("failed to close parquet writer")?,
            probe,
        ))
    }

    async fn commit(&mut self, data_files: Vec<iceberg::spec::DataFile>) -> Result<()> {
        if data_files.is_empty() {
            return Ok(());
        }
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            match self.commit_once(data_files.clone()).await {
                Ok(table) => {
                    self.table = table;
                    return Ok(());
                }
                Err(err) => {
                    // Files are already on S3; retrying only replays the metadata commit.
                    if attempt >= self.cfg.commit_retries.max(1) {
                        return Err(err).context(format!(
                            "iceberg commit failed after {attempt} attempts ({} data files orphaned)",
                            data_files.len()
                        ));
                    }
                    let backoff = Duration::from_millis(200 * 2u64.pow(attempt - 1));
                    tracing::warn!(
                        attempt,
                        backoff_ms = backoff.as_millis(),
                        error = ?err,
                        "iceberg commit failed; reloading table metadata and retrying"
                    );
                    tokio::time::sleep(backoff).await;
                    // A conflict means somebody else advanced the metadata: adopt theirs.
                    self.reload().await?;
                }
            }
        }
    }

    async fn commit_once(&self, data_files: Vec<iceberg::spec::DataFile>) -> Result<Table> {
        let tx = Transaction::new(&self.table);
        let action = tx.fast_append().add_data_files(data_files);
        let tx = action
            .apply(tx)
            .map_err(|e| anyhow::anyhow!(e))
            .context("failed to apply fast-append action")?;

        tx.commit(self.catalog.as_ref())
            .map_err(|e| anyhow::anyhow!(e))
            .await
            .with_context(|| format!("failed to commit snapshot to {}", self.ident))
    }
}

/// The dedup probe's read side: the catalog-backed provider, which re-loads the table
/// metadata on every scan so a probe sees what is committed right now — including the
/// snapshots this process wrote a moment ago and the ones its replicas wrote. (The static
/// provider would pin the snapshot of the moment the sink was built.)
async fn probe_view_provider(
    catalog: &Arc<dyn Catalog>,
    cfg: &IcebergConfig,
) -> Result<Arc<dyn TableProvider>> {
    let df_catalog = IcebergCatalogProvider::try_new(catalog.clone())
        .await
        .context("failed to expose the iceberg catalog to datafusion")?;
    let schema = df_catalog
        .schema(&cfg.namespace)
        .with_context(|| format!("datafusion does not see namespace {}", cfg.namespace))?;
    schema.table(&cfg.table).await?.with_context(|| {
        format!(
            "datafusion does not see table {}.{}",
            cfg.namespace, cfg.table
        )
    })
}

/// `total-data-files` of the current snapshot summary, which Iceberg maintains on every
/// append — reading it costs nothing, unlike counting manifest entries.
fn total_data_files(table: &Table) -> usize {
    table
        .metadata()
        .current_snapshot()
        .and_then(|snapshot| {
            snapshot
                .summary()
                .additional_properties
                .get("total-data-files")
        })
        .and_then(|count| count.parse().ok())
        .unwrap_or(0)
}

/// The partition directories of a data file path, e.g.
/// `exchange=BINANCE/trade_ts_day=2026-08-07`.
fn partition_dirs(path: &str) -> String {
    path.split('/')
        .filter(|segment| segment.contains('='))
        .collect::<Vec<_>>()
        .join("/")
}

fn catalog_props(cfg: &IcebergConfig) -> HashMap<String, String> {
    let mut props = HashMap::new();
    props.insert(REST_CATALOG_PROP_URI.to_string(), cfg.uri.clone());
    props.insert(
        REST_CATALOG_PROP_WAREHOUSE.to_string(),
        cfg.warehouse.clone(),
    );
    props.insert(S3_ENDPOINT.to_string(), cfg.s3_endpoint.clone());
    props.insert(S3_REGION.to_string(), cfg.s3_region.clone());
    props.insert(S3_ACCESS_KEY_ID.to_string(), cfg.s3_access_key_id.clone());
    props.insert(
        S3_SECRET_ACCESS_KEY.to_string(),
        cfg.s3_secret_access_key.clone(),
    );
    props.insert(
        S3_PATH_STYLE_ACCESS.to_string(),
        cfg.s3_path_style.to_string(),
    );
    if let Some(credential) = &cfg.credential {
        props.insert("credential".to_string(), credential.clone());
    }
    props
}

/// Creates the namespace/table when missing; otherwise loads the existing table.
async fn ensure_table(
    catalog: &Arc<dyn Catalog>,
    ident: &TableIdent,
    cfg: &IcebergConfig,
) -> Result<Table> {
    match catalog.load_table(ident).await {
        Ok(table) => Ok(table),
        // Catalogs differ here: Lakekeeper answers `TableNotFound` for a missing table,
        // the memory catalog answers `NamespaceNotFound` when the namespace is missing.
        Err(err)
            if matches!(
                err.kind(),
                iceberg::ErrorKind::TableNotFound | iceberg::ErrorKind::NamespaceNotFound
            ) =>
        {
            tracing::info!(table = %ident, "table not found; creating it");
            if !catalog
                .namespace_exists(ident.namespace())
                .await
                .context("failed to check namespace existence")?
            {
                catalog
                    .create_namespace(ident.namespace(), HashMap::new())
                    .await
                    .with_context(|| format!("failed to create namespace {}", ident.namespace()))?;
            }

            let creation = TableCreation::builder()
                .name(ident.name().to_string())
                .schema(schema::table_schema().context("failed to build trades schema")?)
                .partition_spec(schema::unbound_partition_spec())
                .properties([(
                    "write.parquet.compression-codec".to_string(),
                    cfg.parquet_compression.clone(),
                )])
                .build();

            catalog
                .create_table(ident.namespace(), creation)
                .await
                .with_context(|| format!("failed to create table {ident}"))
        }
        Err(err) => Err(anyhow::anyhow!(err).context(format!("failed to load table {ident}"))),
    }
}

fn writer_properties(codec: &str) -> WriterProperties {
    WriterProperties::builder()
        .set_writer_version(WriterVersion::PARQUET_2_0)
        .set_compression(parse_codec(codec))
        .build()
}

fn parse_codec(codec: &str) -> ParquetCompression {
    match codec.trim().to_ascii_lowercase().as_str() {
        "" | "none" | "uncompressed" => ParquetCompression::UNCOMPRESSED,
        "snappy" => ParquetCompression::SNAPPY,
        "gzip" => ParquetCompression::GZIP(Default::default()),
        "lz4" | "lz4_raw" | "lz4raw" => ParquetCompression::LZ4_RAW,
        // Level 3 is a sane write-heavy default (zstd -3 ~= snappy speed, far better ratio).
        "zstd" => ZstdLevel::try_new(3)
            .map(ParquetCompression::ZSTD)
            .unwrap_or(ParquetCompression::ZSTD(ZstdLevel::default())),
        other => {
            tracing::warn!(codec = other, "unknown parquet codec; falling back to zstd");
            ParquetCompression::ZSTD(ZstdLevel::default())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_parsing_covers_the_useful_set() {
        assert_eq!(parse_codec("snappy"), ParquetCompression::SNAPPY);
        assert_eq!(parse_codec("ZSTD"), parse_codec("zstd"));
        assert_eq!(parse_codec(""), ParquetCompression::UNCOMPRESSED);
        assert_ne!(parse_codec("zstd"), ParquetCompression::SNAPPY);
    }

    #[test]
    fn catalog_props_never_leak_secret_values_into_keys() {
        let cfg = IcebergConfig {
            uri: "http://lk/catalog".into(),
            warehouse: "wh".into(),
            credential: None,
            namespace: "trades".into(),
            table: "t".into(),
            s3_endpoint: "http://s3".into(),
            s3_region: "r".into(),
            s3_access_key_id: "ak".into(),
            s3_secret_access_key: "sk".into(),
            s3_path_style: true,
            parquet_compression: "zstd".into(),
            target_file_size: 1024,
            commit_retries: 3,
        };
        let props = catalog_props(&cfg);
        assert_eq!(
            props.get("uri").map(String::as_str),
            Some("http://lk/catalog")
        );
        assert_eq!(
            props
                .get(iceberg::io::S3_SECRET_ACCESS_KEY)
                .map(String::as_str),
            Some("sk")
        );
        assert!(!props.contains_key("credential"));
    }

    // ---- dedup probe: does it really prune files? -------------------------------------
    //
    // The probe is only worth its latency if Iceberg drops data files before reading
    // them. Two independent things can go wrong and neither shows up in the *rows* the
    // pipeline writes: a predicate DataFusion cannot translate means `FilterExec` over a
    // full scan (correct, but reads the table on every flush), and a probe that prunes
    // too aggressively silently lets duplicates through. So assert on the files, not just
    // on the rows.

    use datafusion::physical_plan::ExecutionPlan;
    use futures::TryStreamExt;
    use iceberg::memory::{MEMORY_CATALOG_WAREHOUSE, MemoryCatalogBuilder};
    use iceberg_datafusion::physical_plan::IcebergTableScan;

    /// 2026-08-07T00:00:00Z, so `day` 0 and 1 land in two distinct `trade_ts_day=` dirs.
    const BASE_MS: u64 = 1_786_060_800_000;
    const DAY_MS: u64 = 86_400_000;
    const DAY_0_DIR: &str = "trade_ts_day=2026-08-07";
    const DAY_1_DIR: &str = "trade_ts_day=2026-08-08";

    fn evt(trade_id: u64, exchange: &str, day: u64, offset_ms: u64) -> TradeEvent {
        let ms = BASE_MS + day * DAY_MS + offset_ms;
        TradeEvent {
            event_time: ms,
            symbol: "BTCUSDT".into(),
            exchange: exchange.into(),
            trade_id,
            price: 1.0,
            quantity: 1.0,
            trade_time: ms,
            is_buyer_maker: false,
            is_best_price_match: true,
        }
    }

    fn test_config() -> IcebergConfig {
        IcebergConfig {
            uri: "http://unused.invalid/catalog".into(),
            warehouse: "unused".into(),
            credential: None,
            namespace: "trades".into(),
            table: "trades_mem".into(),
            s3_endpoint: "http://unused.invalid".into(),
            s3_region: "unused".into(),
            s3_access_key_id: "unused".into(),
            s3_secret_access_key: "unused".into(),
            s3_path_style: true,
            parquet_compression: "zstd".into(),
            target_file_size: 8 * 1024 * 1024,
            commit_retries: 1,
        }
    }

    async fn memory_sink(catalog: Arc<dyn Catalog>) -> IcebergSink {
        IcebergSink::with_catalog(catalog, test_config())
            .await
            .expect("sink auto-creates the table")
    }

    async fn memory_catalog() -> Arc<dyn Catalog> {
        Arc::new(
            MemoryCatalogBuilder::default()
                .load(
                    "memory",
                    HashMap::from([(MEMORY_CATALOG_WAREHOUSE.to_string(), "memory://wh".into())]),
                )
                .await
                .expect("memory catalog"),
        )
    }

    fn table_arrow_schema(sink: &IcebergSink) -> Arc<arrow_schema::Schema> {
        crate::schema::arrow_schema(sink.table.metadata().current_schema()).unwrap()
    }

    fn incoming_batch(sink: &IcebergSink, rows: &[TradeEvent]) -> RecordBatch {
        crate::schema::to_record_batch(&table_arrow_schema(sink), rows).unwrap()
    }

    fn plan_text(plan: &dyn ExecutionPlan) -> String {
        let mut s = datafusion::physical_plan::displayable(plan)
            .indent(true)
            .to_string();
        for child in plan.children() {
            s.push_str(&plan_text(&**child));
        }
        s
    }

    fn iceberg_scans(plan: &dyn ExecutionPlan) -> Vec<&IcebergTableScan> {
        let mut scans: Vec<&IcebergTableScan> = plan
            .as_any()
            .downcast_ref::<IcebergTableScan>()
            .into_iter()
            .collect();
        for child in plan.children() {
            scans.extend(iceberg_scans(&**child));
        }
        scans
    }

    /// What the probe really does, read off the physical plan of the real query: the
    /// predicate it pushes into Iceberg and the table snapshot it reads.
    ///
    /// The snapshot matters: pruning can only ever pick files out of the files that
    /// snapshot has, so `table` — not `IcebergSink::table()` — is the denominator.
    struct ProbeView {
        predicate: iceberg::expr::Predicate,
        table: Table,
    }

    impl ProbeView {
        /// Files this probe opens: the pruned subset of `Self::visible_files`.
        async fn opened_files(&self) -> Vec<String> {
            files_of(&self.table, Some(self.predicate.clone())).await
        }

        /// Files the probe could have opened, i.e. everything its snapshot holds.
        async fn visible_files(&self) -> Vec<String> {
            files_of(&self.table, None).await
        }
    }

    async fn probe_view(sink: &IcebergSink, rows: &[TradeEvent]) -> ProbeView {
        let plan = sink
            .dedup_probe(&incoming_batch(sink, rows), &table_arrow_schema(sink))
            .await
            .unwrap()
            .0
            .create_physical_plan()
            .await
            .unwrap();
        let text = plan_text(&*plan);
        let scans = iceberg_scans(&*plan);
        assert_eq!(
            scans.len(),
            1,
            "probe should read the iceberg table once:\n{text}"
        );
        ProbeView {
            predicate: scans[0].predicates().cloned().unwrap_or_else(|| {
                panic!("probe pushed no predicate down: every flush reads every file\n{text}")
            }),
            table: scans[0].table().clone(),
        }
    }

    /// Data files the probe would open for `rows`, as paths.
    async fn probe_files(sink: &IcebergSink, rows: &[TradeEvent]) -> Vec<String> {
        probe_view(sink, rows).await.opened_files().await
    }

    async fn files_of(table: &Table, predicate: Option<iceberg::expr::Predicate>) -> Vec<String> {
        let mut builder = table.scan().select_all();
        if let Some(p) = predicate {
            builder = builder.with_filter(p);
        }
        builder
            .build()
            .unwrap()
            .plan_files()
            .await
            .unwrap()
            .map_ok(|task| task.data_file_path().to_string())
            .try_collect()
            .await
            .unwrap()
    }

    /// Rows the anti-join keeps, i.e. rows that would actually be written.
    async fn surviving_rows(sink: &IcebergSink, rows: &[TradeEvent]) -> Vec<u64> {
        let incoming = incoming_batch(sink, rows);
        let out = sink
            .deduped_batch(&incoming, &table_arrow_schema(sink))
            .await
            .unwrap()
            .0;
        out.iter()
            .flat_map(|b| {
                let ids = b
                    .column_by_name("trade_id")
                    .unwrap()
                    .as_any()
                    .downcast_ref::<arrow_array::Int64Array>()
                    .unwrap();
                (0..ids.len()).map(|i| ids.value(i) as u64)
            })
            .collect()
    }

    /// The predicate must reach the Iceberg scan: one range per `(exchange, day)` group of
    /// the micro-batch, otherwise the probe is a full-table read wearing a filter.
    #[tokio::test]
    async fn probe_pushes_one_pruning_range_per_exchange_and_day() {
        let sink = memory_sink(memory_catalog().await).await;
        let predicate = probe_view(
            &sink,
            &[
                evt(0, "BINANCE", 0, 0),
                evt(1, "BINANCE", 0, 1),
                evt(0, "OKX", 1, 0),
            ],
        )
        .await
        .predicate
        .to_string();

        assert_eq!(predicate.matches("exchange = ").count(), 2, "{predicate}");
        assert_eq!(predicate.matches("trade_ts >= ").count(), 2, "{predicate}");
        assert_eq!(predicate.matches("trade_ts <= ").count(), 2, "{predicate}");
        assert!(
            predicate.contains("BINANCE") && predicate.contains("OKX"),
            "{predicate}"
        );
    }

    /// The point of the pruning expression: with 4 committed files, a probe opens the one
    /// file its `(exchange, day, trade_ts)` range can possibly match — partition pruning
    /// for the other exchange/day, and per-file `trade_ts` min/max pruning for the two
    /// files that share the `BINANCE/2026-08-07` partition. And the row it does open still
    /// has to be found there, or pruning would be silently wrong.
    #[tokio::test]
    async fn probe_opens_only_the_files_it_could_match() {
        let catalog = memory_catalog().await;
        let mut writer = memory_sink(catalog.clone()).await;

        // Two snapshots, the second one extending the SAME partition with a disjoint
        // trade_ts range.
        writer
            .write_batch(&[
                evt(0, "BINANCE", 0, 0),
                evt(1, "BINANCE", 0, 1),
                evt(0, "OKX", 0, 0),
                evt(0, "BINANCE", 1, 0),
            ])
            .await
            .unwrap();
        writer
            .write_batch(&[evt(50, "BINANCE", 0, 50), evt(51, "BINANCE", 0, 51)])
            .await
            .unwrap();

        // A second sink == a freshly started batcher: its probe view of the table now
        // includes the 4 committed files.
        let sink = memory_sink(catalog).await;
        let all = files_of(sink.table(), None).await;
        assert_eq!(
            all.len(),
            4,
            "one file per written partition, plus the extension"
        );

        // Duplicate of a committed row: exactly the file holding it survives pruning, and
        // the anti-join finds the match there.
        let dup = probe_files(&sink, &[evt(0, "BINANCE", 0, 0)]).await;
        assert_eq!(
            dup.len(),
            1,
            "1 of 4 files probed, not a full scan: {all:?}"
        );
        assert!(
            dup[0].contains("exchange=BINANCE") && dup[0].contains(DAY_0_DIR),
            "{dup:?}"
        );
        assert_eq!(
            surviving_rows(&sink, &[evt(0, "BINANCE", 0, 0)]).await,
            Vec::<u64>::new(),
            "the pruned file set still contains the duplicate"
        );

        // Same exchange and day, but a timestamp only the *second* BINANCE/07 file can
        // contain: per-file min/max pruning, one file opened, and nothing matches it.
        let far = probe_files(&sink, &[evt(99, "BINANCE", 0, 51)]).await;
        assert_eq!(
            far.len(),
            1,
            "the other BINANCE/{DAY_0_DIR} file is range-pruned"
        );
        assert_ne!(
            far[0], dup[0],
            "a different single file than the duplicate probe"
        );
        assert_eq!(
            surviving_rows(&sink, &[evt(99, "BINANCE", 0, 51)]).await,
            vec![99]
        );

        // Same exchange, other day: the day transform alone picks the file, and the
        // duplicate living there is still found.
        let other_day = probe_files(&sink, &[evt(0, "BINANCE", 1, 0)]).await;
        assert_eq!(other_day.len(), 1, "day partition pruning");
        assert!(other_day[0].contains(DAY_1_DIR), "{other_day:?}");
        assert_eq!(
            surviving_rows(&sink, &[evt(0, "BINANCE", 1, 0)]).await,
            Vec::<u64>::new(),
            "cross-day duplicate still matched, pruning did not hide it"
        );

        // A partition that does not exist at all: zero files opened, row kept.
        let none = probe_files(&sink, &[evt(7, "OKX", 1, 5)]).await;
        assert!(
            none.is_empty(),
            "unwritten partition opens no file: {none:?}"
        );
        assert_eq!(surviving_rows(&sink, &[evt(7, "OKX", 1, 5)]).await, vec![7]);
    }

    /// Pruning is load-bearing for *correctness*, not only for cost. The anti-join matches
    /// on `(exchange, symbol, trade_id)` and ignores `trade_ts`, so a new trade reusing an
    /// id already stored for that exchange/symbol on another day is written only because
    /// the probe never opens that other day's file. Widen the probe (drop the `trade_ts`
    /// bounds, or push no predicate at all) and this row is dropped *and acked*: silent
    /// data loss, not a slow query.
    #[tokio::test]
    async fn same_trade_id_on_another_day_is_not_a_duplicate() {
        let catalog = memory_catalog().await;
        let mut writer = memory_sink(catalog.clone()).await;
        writer
            .write_batch(&[evt(7, "BINANCE", 0, 0)])
            .await
            .unwrap();
        let sink = memory_sink(catalog).await;

        let opened = probe_files(&sink, &[evt(7, "BINANCE", 1, 0)]).await;
        assert!(
            opened.is_empty(),
            "the BINANCE/{DAY_0_DIR} file must be pruned for the right reason: {opened:?}"
        );
        assert_eq!(
            surviving_rows(&sink, &[evt(7, "BINANCE", 1, 0)]).await,
            vec![7],
            "same id, other day: kept because the day-0 file was never opened"
        );
    }

    /// What every flush logs (`probe_files=N of M`): the files the probe planned to read
    /// against the files the snapshot it read holds. Both halves come from the same
    /// snapshot, and the counters are what make a pruning regression visible in production
    /// instead of only in here.
    #[tokio::test]
    async fn flush_logs_probe_files_read_against_files_in_the_table() {
        let catalog = memory_catalog().await;
        let mut sink = memory_sink(catalog).await;

        let first = sink
            .write_batch(&[evt(0, "BINANCE", 0, 0), evt(0, "OKX", 0, 0)])
            .await
            .unwrap();
        assert_eq!(first.files, 2);
        assert_eq!(
            probe_counts(first.probe),
            (0, 0, 0),
            "no snapshot to probe yet"
        );
        assert_eq!(first.probe.bytes_opened, 0);

        // Same partition and overlapping timestamp: the one BINANCE file must be read,
        // the OKX one must not, and M must have grown by the 2 files just committed.
        let second = sink.write_batch(&[evt(1, "BINANCE", 0, 0)]).await.unwrap();
        assert_eq!(second.files, 1);
        assert_eq!(probe_counts(second.probe), (1, 1, 2));
        assert!(second.probe.bytes_opened > 0);

        // A micro-batch spread over two exchanges and two days opens one file per touched
        // partition — including all of them, so the counter is not vacuously small.
        let third = sink
            .write_batch(&[
                evt(2, "BINANCE", 0, 0),
                evt(1, "OKX", 0, 0),
                evt(0, "BINANCE", 1, 0),
            ])
            .await
            .unwrap();
        assert_eq!(probe_counts(third.probe), (3, 2, 3));
    }

    fn probe_counts(probe: ProbeStats) -> (usize, usize, usize) {
        (
            probe.files_opened,
            probe.partitions_opened,
            probe.data_files,
        )
    }

    /// Both behaviours the dedup probe has to get right, measured on the same three
    /// redelivered trades:
    ///
    /// 1. **selective** — it opens only the files the incoming `(exchange, day, trade_ts)`
    ///    ranges can match, and
    /// 2. **complete** — within what it opens, it still matches every duplicate.
    ///
    /// Which files those are depends on the snapshot the probe reads, and `icetbl` is
    /// registered once in [`IcebergSink::with_catalog`] with a static provider that
    /// `commit`/`reload` never refresh. So behaviour 2 holds for a (re)started batcher and
    /// not for the process that wrote the rows a micro-batch earlier — the redelivered
    /// rows of an already-committed micro-batch are written again. This last assertion is
    /// red until `icetbl` is re-registered from `self.table` in `commit`/`reload` (or the
    /// catalog-backed `IcebergTableProvider` is registered instead, which additionally sees
    /// other batcher replicas).
    #[tokio::test]
    async fn redelivered_rows_are_dropped_from_the_files_the_probe_can_see() {
        let catalog = memory_catalog().await;
        let mut batcher = memory_sink(catalog.clone()).await;

        batcher
            .write_batch(&[
                evt(0, "BINANCE", 0, 0),
                evt(1, "BINANCE", 0, 1),
                evt(0, "OKX", 0, 0),
            ])
            .await
            .unwrap();
        batcher
            .write_batch(&[evt(2, "BINANCE", 0, 40), evt(100, "BINANCE", 1, 0)])
            .await
            .unwrap();
        let committed = files_of(batcher.table(), None).await;
        assert_eq!(committed.len(), 4, "one file per touched partition");

        // Two of these share one partition (BINANCE/07), so the probe's range for that
        // group spans both of its files.
        let redelivered = [
            evt(0, "BINANCE", 0, 0),
            evt(2, "BINANCE", 0, 40),
            evt(100, "BINANCE", 1, 0),
        ];

        // ---- behaviour 1 + 2, on a batcher that starts now --------------------------
        let restarted = memory_sink(catalog).await;
        let view = probe_view(&restarted, &redelivered).await;
        assert_eq!(view.visible_files().await.len(), 4);

        let opened = view.opened_files().await;
        assert_eq!(
            opened.len(),
            3,
            "selective: the OKX file is pruned, the three BINANCE ones are not: {committed:?}"
        );
        assert!(
            opened.iter().all(|path| path.contains("exchange=BINANCE")),
            "{opened:?}"
        );
        assert_eq!(
            surviving_rows(&restarted, &redelivered).await,
            Vec::<u64>::new(),
            "complete: pruning down to 3 of 4 files still matches every redelivered trade"
        );

        // ---- the same rows, one micro-batch later, in the process that wrote them ---
        let view = probe_view(&batcher, &redelivered).await;
        let visible = view.visible_files().await;
        let opened = view.opened_files().await;
        let kept = surviving_rows(&batcher, &redelivered).await;
        assert!(
            kept.is_empty(),
            "complete: the running batcher sees {} of the {} committed files, opened {} of \
             those and kept {kept:?} of 3 redelivered trades",
            visible.len(),
            committed.len(),
            opened.len(),
        );
    }
}
