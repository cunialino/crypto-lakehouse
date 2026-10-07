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

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures::TryFutureExt;
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
}

pub struct IcebergSink {
    catalog: Arc<dyn Catalog>,
    ident: TableIdent,
    table: Table,
    /// Process-wide so the embedded file counter never restarts and file names stay unique
    /// across flushes; the uuid keeps them unique across restarts.
    file_name_gen: DefaultFileNameGenerator,
    cfg: IcebergConfig,
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
        let data_files = self.write_files(rows).await?;
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
        })
    }

    async fn write_files(&self, rows: &[TradeEvent]) -> Result<Vec<iceberg::spec::DataFile>> {
        let metadata = self.table.metadata();
        let iceberg_schema = metadata.current_schema().clone();
        let arrow_schema = schema::arrow_schema(&iceberg_schema)?;
        let partition_spec = metadata.default_partition_spec().clone();

        let batch = schema::to_record_batch(&arrow_schema, rows)?;
        let splitter = iceberg::arrow::RecordBatchPartitionSplitter::try_new_with_computed_values(
            iceberg_schema.clone(),
            partition_spec.clone(),
        )
        .context("failed to build partition splitter")?;
        let partitions = splitter
            .split(&batch)
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
        writer
            .close()
            .await
            .context("failed to close parquet writer")
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
}
