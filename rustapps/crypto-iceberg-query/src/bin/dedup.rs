//! Duplicate audit / deduplicated rewrite for one `trades` table over a `trade_ts` range.
//!
//! Why this exists: `crypto-batcher` is at-least-once and its inline dedup deliberately
//! spans a *single* micro-batch (see `crypto_batcher::batcher::Accumulator`). Rows that
//! get redelivered after a *committed* batch — commit succeeded, ack failed, or a
//! replayed range — therefore land in the table twice, and something out of band has to
//! remove them. That slot was the Spark maintenance job; this is the Rust/DataFusion
//! version of it.
//!
//! Plan choice — `ROW_NUMBER() OVER (PARTITION BY exchange, symbol, trade_id ...)`
//! instead of a hash anti-join against a `GROUP BY` key set:
//!
//! * DataFusion 53.1 requires window input sorted on `(partition_by, order_by)`, inserts a
//!   `SortExec`, and that sort spills to disk (external sort). The window operator then
//!   streams each key group with O(1) state, so peak memory tracks the *spill buffer*,
//!   not the number of distinct keys.
//! * The anti-join shape needs the kept-key set from `GROUP BY exchange, symbol,
//!   trade_id` first: a hash table over every distinct key in range — the operator family
//!   this repo already measured as not fully pool-accounted in 53.1 (see
//!   `iceberg_session`). Its hash-join side does spill; the build-side aggregate does not
//!   save you.
//!
//! Reading the whole range twice (sort input + write output) is the real cost, so run it
//! per day-partition on a schedule, never inline in the writer.
//!
//! ```text
//! # audit only (read-only, safe next to the live writer)
//! crypto-dedup --table trades.trades_rust_test '2026-10-08 00:00:00' '2026-10-09 00:00:00'
//!
//! # rewrite into a table with the identical schema/partition spec. Create it first with
//! # `crypto-batcher --ensure-only` (ICEBERG_TABLE=<target>), which reuses schema.rs.
//! crypto-dedup --table trades.trades --insert-into trades.trades_dedup \
//!     '2026-10-01 00:00:00' '2026-10-02 00:00:00'
//! ```

use anyhow::{Context, Result};
use clap::Parser;
use datafusion::arrow::array::{Int64Array, UInt64Array};
use std::fmt::Write as _;
use std::time::Instant;

use crypto_iceberg_query::{
    CATALOG, EngineOptions, TRADE_COLUMNS, checked_qualified_name, iceberg_session, peak_rss_mib,
};

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Source table as `<namespace>.<table>`.
    #[arg(long, default_value = "trades.trades")]
    table: String,
    /// Inclusive lower bound for `trade_ts`.
    start: String,
    /// Exclusive upper bound for `trade_ts` (a day-partition boundary prunes best).
    end: String,
    /// Write the deduplicated rows here instead of only auditing. Must already exist.
    #[arg(long)]
    insert_into: Option<String>,
    /// DataFusion pool cap in MiB. Accounting limit, not an RSS limit.
    #[arg(long, default_value_t = 2048)]
    memory_mb: usize,
    /// Where DataFusion spills the sort. Needs roughly the sorted input size.
    #[arg(long, default_value = "/tmp/crypto-dedup")]
    temp_dir: String,
    #[arg(long, default_value_t = 8)]
    target_partitions: usize,
    /// Print the physical plan and exit (check pruning + that the sort is there).
    #[arg(long)]
    explain: bool,
}

/// The dedup projection: every data column plus `rn`, with the range filter pushed into
/// the CTE so both the audit and the rewrite share one plan shape.
///
/// `source` is fully qualified (`lk.trades.trades`) so the tests can point it at a
/// `MemTable` without dragging in a catalog.
fn ranked_cte(source: &str, start: &str, end: &str) -> String {
    let mut sql = String::new();
    writeln!(
        sql,
        "WITH ranked AS (\
             SELECT {TRADE_COLUMNS}, \
                    row_number() OVER (\
                        PARTITION BY exchange, symbol, trade_id \
                        ORDER BY event_ts, trade_ts\
                    ) AS rn \
             FROM {source} \
             WHERE trade_ts >= TIMESTAMP '{start}' AND trade_ts < TIMESTAMP '{end}'\
         )"
    )
    .expect("writing to a String cannot fail");
    sql
}

/// Read a single-row count out of a result batch.
///
/// DataFusion spells counts `UInt64` for a plain `count(*)` but `Int64` once the
/// aggregate goes through other paths, so accept both instead of guessing per release.
fn count_from(batch: &datafusion::arrow::record_batch::RecordBatch, column: &str) -> Result<u64> {
    let array = batch
        .column_by_name(column)
        .with_context(|| format!("no `{column}` in result"))?;
    if let Some(counts) = array.as_any().downcast_ref::<UInt64Array>() {
        anyhow::ensure!(counts.len() == 1, "`{column}`: expected one row");
        return Ok(counts.value(0));
    }
    if let Some(counts) = array.as_any().downcast_ref::<Int64Array>() {
        anyhow::ensure!(counts.len() == 1, "`{column}`: expected one row");
        return Ok(counts.value(0).max(0) as u64);
    }
    anyhow::bail!("`{column}` is {:?}, expected a count", array.data_type());
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    tracing_subscriber::fmt::init();

    let table = checked_qualified_name(&args.table)?.to_string();
    let target = args
        .insert_into
        .as_deref()
        .map(checked_qualified_name)
        .transpose()?
        .map(str::to_string);

    let ctx = iceberg_session(&EngineOptions {
        memory_mb: args.memory_mb,
        temp_dir: Some(args.temp_dir.clone().into()),
        ..EngineOptions::default()
    })
    .await?;
    eprintln!(
        "[info] pool {} MiB, spill dir {}, range [{}, {})",
        args.memory_mb, args.temp_dir, args.start, args.end
    );

    let sql = match &target {
        None => format!(
            "{} SELECT count(*) AS rows_in_range, \
                    count(*) FILTER (WHERE rn > 1) AS duplicate_rows \
             FROM ranked",
            ranked_cte(&format!("{CATALOG}.{table}"), &args.start, &args.end)
        ),
        Some(into) => format!(
            "{} INSERT INTO {CATALOG}.{into} SELECT {TRADE_COLUMNS} FROM ranked WHERE rn = 1",
            ranked_cte(&format!("{CATALOG}.{table}"), &args.start, &args.end)
        ),
    };

    let df = ctx.sql(&sql).await.context("failed to plan dedup query")?;

    if args.explain {
        let explain = df.clone().explain(false, true)?;
        explain.show().await.context("failed to show plan")?;
        return Ok(());
    }

    let start = Instant::now();
    let batches = df.collect().await.context("failed to run dedup query")?;
    let elapsed = start.elapsed();

    match &target {
        None => {
            let batch = batches.first().context("dedup audit returned no rows")?;
            let total = count_from(batch, "rows_in_range")?;
            let duplicates = count_from(batch, "duplicate_rows")?;
            let pct = if total == 0 {
                0.0
            } else {
                duplicates as f64 * 100.0 / total as f64
            };
            println!("rows in range : {total}");
            println!("duplicate rows: {duplicates} ({pct:.4}%)");
        }
        Some(into) => {
            let written = batches
                .iter()
                .map(|batch| count_from(batch, "Rows Executed").unwrap_or(0))
                .sum::<u64>();
            println!("rewrote {written} rows into {CATALOG}.{into}");
            println!("note: the source is untouched; swapping is a separate, manual step");
        }
    }

    println!("elapsed   : {elapsed:?}");
    println!("peak rss  : {} MiB", peak_rss_mib());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::count_from;
    use super::ranked_cte;
    use datafusion::arrow::array::{
        BooleanArray, Float64Array, Int64Array, StringArray, TimestampMicrosecondArray,
    };
    use datafusion::arrow::datatypes::{DataType, Field, Schema, TimeUnit};
    use datafusion::arrow::record_batch::RecordBatch;
    use datafusion::datasource::MemTable;
    use datafusion::prelude::SessionContext;
    use std::sync::Arc;

    /// Seconds since epoch -> micros, so the fixtures line up with the SQL literals below
    /// (day 0 = 1970-01-01, which is what the range filter uses).
    fn ts(day: i64, secs: i64) -> i64 {
        (day * 86_400 + secs) * 1_000_000
    }

    /// Same 11 columns / timestamp units as `crypto_batcher::schema` (Iceberg spells the
    /// UTC timezone `+00:00`), so a drift in the contract fails here too.
    fn schema() -> Arc<Schema> {
        let tz: Arc<str> = Arc::from("+00:00");
        Arc::new(Schema::new(vec![
            Field::new("event_time", DataType::Int64, false),
            Field::new("symbol", DataType::Utf8, false),
            Field::new("exchange", DataType::Utf8, false),
            Field::new("trade_id", DataType::Int64, false),
            Field::new("price", DataType::Float64, false),
            Field::new("quantity", DataType::Float64, false),
            Field::new("trade_time", DataType::Int64, false),
            Field::new("is_buyer_maker", DataType::Boolean, false),
            Field::new("is_best_price_match", DataType::Boolean, false),
            Field::new(
                "trade_ts",
                DataType::Timestamp(TimeUnit::Microsecond, Some(tz.clone())),
                false,
            ),
            Field::new(
                "event_ts",
                DataType::Timestamp(TimeUnit::Microsecond, Some(tz)),
                false,
            ),
        ]))
    }

    /// Fixture rows: `(trade_id, exchange, symbol, event_ts offset in seconds)`.
    /// Everything lands in `trade_ts` day 0 except the optional `extra_out_of_range` row.
    fn ctx_with(rows: &[(i64, &str, &str, i64)], extra_out_of_range: bool) -> SessionContext {
        let mut ids = Vec::new();
        let mut exchanges = Vec::new();
        let mut symbols = Vec::new();
        let mut trade_ts = Vec::new();
        let mut event_ts = Vec::new();

        let mut push = |id: i64, ex: &str, sym: &str, day: i64, ev: i64| {
            ids.push(id);
            exchanges.push(ex.to_string());
            symbols.push(sym.to_string());
            trade_ts.push(ts(day, 3_600));
            event_ts.push(ts(day, ev));
        };

        for &(id, ex, sym, ev) in rows {
            push(id, ex, sym, 0, ev);
        }
        if extra_out_of_range {
            push(999, "BINANCE", "BTCUSDT", 5, 60);
        }
        let n = ids.len();

        let batch = RecordBatch::try_new(
            schema(),
            vec![
                Arc::new(Int64Array::from(ids.clone())), // event_time: payload-irrelevant
                Arc::new(StringArray::from(symbols)),
                Arc::new(StringArray::from(exchanges)),
                Arc::new(Int64Array::from(ids)),
                Arc::new(Float64Array::from(vec![1.0; n])),
                Arc::new(Float64Array::from(vec![1.0; n])),
                Arc::new(Int64Array::from(vec![0; n])),
                Arc::new(BooleanArray::from(vec![false; n])),
                Arc::new(BooleanArray::from(vec![true; n])),
                Arc::new(TimestampMicrosecondArray::from(trade_ts).with_timezone("+00:00")),
                Arc::new(TimestampMicrosecondArray::from(event_ts).with_timezone("+00:00")),
            ],
        )
        .expect("fixture matches schema");

        let ctx = SessionContext::new();
        ctx.register_table(
            "trades",
            Arc::new(MemTable::try_new(schema(), vec![vec![batch]]).unwrap()),
        )
        .expect("register MemTable");
        ctx
    }

    const DAY0: (&str, &str) = ("1970-01-01 00:00:00", "1970-01-02 00:00:00");

    /// Duplicates are keyed on `(exchange, symbol, trade_id)`: a repeated id on another
    /// exchange is a real trade, and rows outside the `trade_ts` range must not be
    /// counted (that is what makes a per-day CronJob cheap).
    #[tokio::test]
    async fn audit_counts_only_real_duplicates_in_range() {
        let ctx = ctx_with(
            &[
                (100, "BINANCE", "BTCUSDT", 60),
                (100, "BINANCE", "BTCUSDT", 300), // redelivery -> redundant
                (101, "BINANCE", "BTCUSDT", 360),
                (100, "OKX", "BTCUSDT", 400), // same id, other exchange -> real
            ],
            true,
        );
        let sql = format!(
            "{} SELECT count(*) AS rows_in_range, \
                    count(*) FILTER (WHERE rn > 1) AS duplicate_rows \
             FROM ranked",
            ranked_cte("trades", DAY0.0, DAY0.1)
        );
        let batches = ctx.sql(&sql).await.unwrap().collect().await.unwrap();
        assert_eq!(batches.len(), 1, "audit must return a single row");
        let batch = &batches[0];

        assert_eq!(
            count_from(batch, "rows_in_range").unwrap(),
            4,
            "out-of-range row leaked in"
        );
        assert_eq!(
            count_from(batch, "duplicate_rows").unwrap(),
            1,
            "expected exactly the redelivered row to be redundant"
        );
    }

    /// The rewrite projection: one row per key, the earliest `event_ts` kept, column list
    /// matching the Iceberg table so `INSERT INTO` cannot write columns out of order.
    #[tokio::test]
    async fn rewrite_keeps_earliest_row_per_key() {
        let ctx = ctx_with(
            &[
                (100, "BINANCE", "BTCUSDT", 300), // the later duplicate must lose
                (100, "BINANCE", "BTCUSDT", 60),
                (101, "BINANCE", "BTCUSDT", 360),
                (100, "OKX", "BTCUSDT", 400),
            ],
            false,
        );
        let sql = format!(
            "{} SELECT exchange, trade_id, event_ts FROM ranked WHERE rn = 1 \
             ORDER BY exchange, trade_id, event_ts",
            ranked_cte("trades", DAY0.0, DAY0.1)
        );
        let batches = ctx.sql(&sql).await.unwrap().collect().await.unwrap();
        let kept: Vec<(String, i64, i64)> = batches
            .iter()
            .flat_map(|b| {
                let col_str = |name: &str| {
                    b.column_by_name(name)
                        .unwrap()
                        .as_any()
                        .downcast_ref::<StringArray>()
                        .unwrap()
                        .clone()
                };
                let col_i64 = |name: &str| {
                    b.column_by_name(name)
                        .unwrap()
                        .as_any()
                        .downcast_ref::<Int64Array>()
                        .unwrap()
                        .clone()
                };
                let col_ts = |name: &str| {
                    b.column_by_name(name)
                        .unwrap()
                        .as_any()
                        .downcast_ref::<TimestampMicrosecondArray>()
                        .unwrap()
                        .clone()
                };
                let (ex, id, ev) = (col_str("exchange"), col_i64("trade_id"), col_ts("event_ts"));
                (0..b.num_rows()).map(move |i| (ex.value(i).to_string(), id.value(i), ev.value(i)))
            })
            .collect();

        assert_eq!(
            kept,
            vec![
                ("BINANCE".to_string(), 100, ts(0, 60)),
                ("BINANCE".to_string(), 101, ts(0, 360)),
                ("OKX".to_string(), 100, ts(0, 400)),
            ],
            "one row per (exchange, symbol, trade_id), the earliest event_ts wins"
        );
    }
}
