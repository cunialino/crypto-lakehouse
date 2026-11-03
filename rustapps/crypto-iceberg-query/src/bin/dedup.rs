//! Duplicate audit / deduplicated rewrite for one `trades` table over a `trade_ts` range.
//!
//! Why this exists: `crypto-batcher` is at-least-once. Its accumulator only dedups within
//! one micro-batch, and its pre-commit anti-join against the table only looks at the
//! `(exchange, trade_ts)` ranges the incoming batch touches. Nothing else removes the
//! duplicates that were written before that existed, or dedups a range that has to be
//! rewritten as a whole. That slot was the Spark maintenance job; this is the
//! Rust/DataFusion version of it.
//!
//! Plan choice: `ROW_NUMBER() OVER (PARTITION BY exchange, symbol, trade_id ...)` rather
//! than a hash anti-join against a `GROUP BY` key set. DataFusion 53.1 requires window
//! input sorted on `(partition_by, order_by)`, so it inserts a `SortExec`, and that sort
//! spills to disk; the window operator then streams each key group with O(1) state, so peak
//! memory tracks the spill buffer instead of the number of distinct keys. The anti-join
//! shape first needs the kept-key set from `GROUP BY exchange, symbol, trade_id`: a hash
//! table over every distinct key in range, which is the operator family this repo already
//! measured as not fully pool-accounted in 53.1 (see `iceberg_session`).
//!
//! Reading the whole range twice (sort input + write output) is the real cost, so run it
//! per day-partition on a schedule, never inline in the writer.
//!
//! The rewrite never touches the source: it appends the kept rows somewhere else, and swapping
//! the two names is a separate, deliberate step.
//!
//! ```text
//! # audit only (read-only, safe next to the live writer)
//! crypto-dedup --table trades.trades '2026-10-08 00:00:00' '2026-10-09 00:00:00'
//!
//! # rewrite into a table with the identical schema/partition spec. Create it first with
//! # `crypto-batcher --ensure-only` (ICEBERG_TABLE=<target>), which reuses schema.rs.
//! crypto-dedup --table trades.trades --insert-into trades.trades_dedup \
//!     '2026-10-01 00:00:00' '2026-10-02 00:00:00' --chunk-rows 1000000
//! ```

use anyhow::{Context, Result};
use clap::Parser;
use std::fmt::Write as _;
use std::time::Instant;

use crypto_iceberg_query::{
    CATALOG, EngineOptions, TRADE_COLUMNS, append, checked_qualified_name, count_from,
    iceberg_session, peak_rss_mib,
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
    /// Rows per commit, for `--insert-into`. Each chunk is one `fast_append` snapshot.
    #[arg(long, default_value_t = 1_000_000)]
    chunk_rows: usize,
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

/// The audit: how many rows are in range, and how many of them are redundant.
fn audit_sql(ctes: &str) -> String {
    format!(
        "{ctes} SELECT count(*) AS rows_in_range, \
                count(*) FILTER (WHERE rn > 1) AS duplicate_rows \
         FROM ranked"
    )
}

/// The rewrite: one row per key, in table column order, ready to stream.
///
/// A plain `SELECT` rather than an `INSERT`: DataFusion 53.1 cannot plan `WITH … INSERT INTO …`
/// (`Query INSERT INTO … not implemented yet`), and the write belongs in the batcher's sink
/// anyway — see [`append`].
fn rewrite_sql(ctes: &str) -> String {
    format!("{ctes} SELECT {TRADE_COLUMNS} FROM ranked WHERE rn = 1")
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

    let ctes = ranked_cte(&format!("{CATALOG}.{table}"), &args.start, &args.end);
    let start = Instant::now();

    match &target {
        None => {
            let df = ctx
                .sql(&audit_sql(&ctes))
                .await
                .context("failed to plan dedup audit")?;
            if args.explain {
                return show_plan(df).await;
            }
            let batches = df.collect().await.context("failed to run dedup audit")?;
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
            let sql = rewrite_sql(&ctes);
            if args.explain {
                let df = ctx
                    .sql(&sql)
                    .await
                    .context("failed to plan dedup rewrite")?;
                return show_plan(df).await;
            }
            let mut sink = append::open(into).await?;
            let written = append::stream_into(&ctx, &mut sink, &sql, args.chunk_rows).await?;
            println!("rewrote {written} deduplicated rows into {CATALOG}.{into}");
            let visible = append::ensure_visible(
                &ctx,
                &format!("{CATALOG}.{into}"),
                &args.start,
                &args.end,
                written,
            )
            .await?;
            println!("visible       : {visible} rows readable in the range");
            println!("note: the source is untouched; swapping is a separate, manual step");
        }
    }

    println!("elapsed   : {:?}", start.elapsed());
    println!("peak rss  : {} MiB", peak_rss_mib());
    Ok(())
}

/// `--explain`: check pruning, and that the sort the window operator needs is really there.
async fn show_plan(df: datafusion::dataframe::DataFrame) -> Result<()> {
    df.clone()
        .explain(false, true)?
        .show()
        .await
        .context("failed to show plan")
}

#[cfg(test)]
mod tests {
    use super::ranked_cte;
    use super::rewrite_sql;
    use crypto_iceberg_query::count_from;
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

    /// The rewrite is what `append::stream_into` commits, so its output has to match the
    /// destination schema exactly — same columns, same order, same types, none nullable. Taken
    /// from the batcher rather than restated here, so a change in `schema.rs` fails this too.
    #[tokio::test]
    async fn rewrite_output_matches_the_destination_schema() {
        let ctx = ctx_with(
            &[
                (100, "BINANCE", "BTCUSDT", 60),
                (100, "BINANCE", "BTCUSDT", 300),
                (101, "BINANCE", "BTCUSDT", 360),
            ],
            false,
        );
        let batch = ctx
            .sql(&rewrite_sql(&ranked_cte("trades", DAY0.0, DAY0.1)))
            .await
            .unwrap()
            .collect()
            .await
            .unwrap()
            .remove(0);
        assert_eq!(batch.num_rows(), 1, "the duplicate survived the rewrite");

        let target =
            crypto_batcher::schema::arrow_schema(&crypto_batcher::schema::table_schema().unwrap())
                .unwrap();
        let got = batch.schema();
        assert_eq!(
            got.fields()
                .iter()
                .map(|f| f.name().as_str())
                .collect::<Vec<_>>(),
            target
                .fields()
                .iter()
                .map(|f| f.name().as_str())
                .collect::<Vec<_>>(),
            "the sink binds by position, so the projection order must be the table's"
        );
        for (want, have) in target.fields().iter().zip(got.fields()) {
            assert_eq!(
                have.data_type(),
                want.data_type(),
                "`{}`: wrong type — `Utf8View` and a missing `+00:00` are the usual drifts",
                want.name()
            );
            assert_eq!(
                have.is_nullable(),
                want.is_nullable(),
                "`{}`: wrong nullability",
                want.name()
            );
        }
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
