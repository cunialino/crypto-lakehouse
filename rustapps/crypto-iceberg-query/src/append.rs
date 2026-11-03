//! Append the output of a query to an Iceberg table using the batcher's own writer.
//!
//! The tools here used to append with a SQL `INSERT`, which is not a workable shape.
//! `iceberg-datafusion` resolves a table's partition source columns *by position*, while
//! DataFusion hoists repeated projection arguments into leading `__common_expr_N` columns: the
//! partition transform is then handed whatever column sits at that index — loudly for
//! `day(trade_ts)` (`Should not call internally for unsupported data type Utf8`), silently and
//! wrongly for `identity(exchange)` whenever both source columns are strings.
//!
//! So query output is streamed instead, re-bound to the destination schema — which turns any
//! drift in type, nullability or column order into a refused write — and committed by
//! [`IcebergSink::append_records`], the same splitter and fanout writer the live pipeline uses.
//! Files then land in the partitions the table's spec describes rather than in whatever the
//! planner happened to derive.
//!
//! Each chunk is one commit, so a chunk that dies costs its own objects as orphans and never
//! the chunks already committed. Both callers select rows by key, so re-running converges.

use anyhow::{Context, Result};
use datafusion::arrow::compute::concat_batches;
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::prelude::SessionContext;
use futures::StreamExt;

use crypto_batcher::{config::IcebergConfig, sink::IcebergSink};

use crate::{CATALOG, checked_qualified_name, count_from};

/// Opens `<namespace>.<table>` with the batcher's writer configuration.
pub async fn open(table: &str) -> Result<IcebergSink> {
    let name = checked_qualified_name(table)?.to_string();
    let (namespace, table) = name
        .split_once('.')
        .context("destination must be `<namespace>.<table>`")?;
    let mut cfg = IcebergConfig::from_env().context("invalid iceberg configuration")?;
    cfg.namespace = namespace.to_string();
    cfg.table = table.to_string();
    IcebergSink::connect(cfg)
        .await
        .with_context(|| format!("failed to open the destination table {CATALOG}.{name}"))
}

/// Streams `sql` into `sink`, committing `chunk_rows` at a time.
///
/// The query has to yield exactly the destination's columns, in order and in its types —
/// `crate::TRADE_COLUMNS` is that list. Bigger chunks mean fewer snapshots and larger files;
/// smaller ones mean less work redone after a failed commit.
pub async fn stream_into(
    ctx: &SessionContext,
    sink: &mut IcebergSink,
    sql: &str,
    chunk_rows: usize,
) -> Result<u64> {
    let schema = sink.arrow_schema()?;
    let mut stream = ctx
        .sql(sql)
        .await
        .context("failed to plan the append query")?
        .execute_stream()
        .await
        .context("failed to start the append query")?;

    let mut buffer: Vec<RecordBatch> = Vec::new();
    let mut buffered = 0usize;
    let mut chunks = 0u64;
    let mut written = 0u64;

    while let Some(batch) = stream.next().await {
        let batch = batch.context("append query failed")?;
        buffer.push(
            RecordBatch::try_new(schema.clone(), batch.columns().to_vec())
                .context("query output does not match the destination schema")?,
        );
        buffered += batch.num_rows();
        if buffered >= chunk_rows {
            written += flush(sink, &mut buffer, &mut chunks).await?;
            buffered = 0;
        }
    }
    if buffered > 0 {
        written += flush(sink, &mut buffer, &mut chunks).await?;
    }
    Ok(written)
}

/// Commits what is buffered as one snapshot.
async fn flush(
    sink: &mut IcebergSink,
    buffer: &mut Vec<RecordBatch>,
    chunks: &mut u64,
) -> Result<u64> {
    let batch = concat_batches(&buffer[0].schema(), &buffer[..])?;
    buffer.clear();
    let stats = sink.append_records(batch).await?;
    *chunks += 1;
    println!(
        "  chunk {chunks}: {} rows, {} files, {} partitions, {} MiB (write {:?}, commit {:?})",
        stats.rows,
        stats.files,
        stats.partitions,
        stats.bytes / (1024 * 1024),
        stats.write_elapsed,
        stats.commit_elapsed,
    );
    Ok(stats.rows as u64)
}

/// Reads `[start, end)` back through the destination's partitions and refuses to pass a write
/// that left rows behind.
///
/// Rows filed under a wrong `day(trade_ts)` are invisible to a time-pruned read, so this is
/// the cheap way to catch a mis-partitioned append instead of discovering it months later as a
/// hole in the table.
pub async fn ensure_visible(
    ctx: &SessionContext,
    table: &str,
    start: &str,
    end: &str,
    written: u64,
) -> Result<u64> {
    let batches = ctx
        .sql(&format!(
            "SELECT count(*) AS visible FROM {table} \
             WHERE trade_ts >= TIMESTAMP '{start}' AND trade_ts < TIMESTAMP '{end}'"
        ))
        .await
        .context("failed to plan the verification read")?
        .collect()
        .await
        .context("failed to read the range back")?;
    let visible = count_from(
        batches
            .first()
            .context("verification read returned no rows")?,
        "visible",
    )?;
    anyhow::ensure!(
        visible >= written,
        "only {visible} of {written} written rows are readable in the range, so some are filed \
         under partition values a pruned read cannot find — audit the table before trusting it"
    );
    Ok(visible)
}
