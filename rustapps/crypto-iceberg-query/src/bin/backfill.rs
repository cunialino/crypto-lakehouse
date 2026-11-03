//! Append the rows one `trades` table is missing, taken from a sibling table that has them:
//! an older table kept for the purpose, a restored snapshot, a re-ingested export. Replaying
//! the source stream is not an option once the gap is older than `tradesstream`'s 72 h
//! retention, so the rows are copied across instead.
//!
//! This is how 2026-06-24 → 2026-10-07 got into `trades.trades`, from the pre-cutover table
//! then named `trades.trades_legacy` — which is dropped, so read the cutover notes in
//! `deploy/docs/rust-batcher-guide.md` before looking for it.
//!
//! The anti-join is what makes this safe to run more than once: rows the target already
//! has are never appended again, so a retried or overlapping run cannot create duplicates —
//! which matters because the target is live (`crypto-batcher` commits into it every
//! `BATCH_FLUSH_INTERVAL_SECS`, and a commit conflict costs a retry, not correctness).
//!
//! ```text
//! # audit only (read-only, the default): what would be appended?
//! crypto-backfill --from <source> --into trades.trades \
//!     '2026-10-06 19:00:00' '2026-10-06 20:00:00'
//!
//! # commit it, one snapshot per million rows
//! crypto-backfill --from <source> --into trades.trades \
//!     '2026-10-06 19:00:00' '2026-10-06 20:00:00' --execute --chunk-rows 1000000
//! ```
//!
//! The source is read as one row per `(exchange, symbol, trade_id)`: the pre-cutover table
//! double-wrote whole days, and a copied day may therefore end up smaller than the source
//! day. `duplicate_rows` in the audit is that excess, not a problem to fix.
//!
//! The schema differences between the pre-cutover table and the batcher's are normalised on
//! the way in rather than copied verbatim — see `CANDIDATE_PROJECTION`.
//!
//! A candidate with a NULL in a column the target requires is skipped rather than defaulted;
//! `rows_skipped_null` reports how many, and `--execute` appends exactly `rows_to_append`.
//!
//! `--execute` appends through `crypto_iceberg_query::append` — the batcher's writer, with the
//! output bound to the destination schema — rather than a SQL `INSERT`, which mis-partitions
//! rows there; that module explains why. `projection_matches_the_target_contract` pins this
//! tool's projection to what the destination expects.

use anyhow::{Context, Result};
use clap::Parser;
use datafusion::arrow::record_batch::RecordBatch;
use std::fmt::Write as _;
use std::time::Instant;

use crypto_iceberg_query::{
    CATALOG, EngineOptions, append, checked_qualified_name, count_from, iceberg_session,
    peak_rss_mib,
};

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Table to copy from, as `<namespace>.<table>`. Required: the only table a caller is
    /// certain to name is the destination, and reading from it would be a no-op at best.
    #[arg(long)]
    from: String,
    /// Table to append to, as `<namespace>.<table>`.
    #[arg(long)]
    into: String,
    /// Inclusive lower bound for `trade_ts`.
    start: String,
    /// Exclusive upper bound for `trade_ts` (a day-partition boundary prunes best).
    end: String,
    /// Widen the target-side partition range by this many days at both ends, so a row that
    /// landed in a neighbouring day partition on one side is still recognised as present.
    #[arg(long, default_value_t = 1)]
    target_margin_days: i64,
    /// Commit the append instead of only reporting it.
    #[arg(long)]
    execute: bool,
    /// Rows per commit. A chunk is one `fast_append` snapshot, so bigger chunks mean fewer
    /// snapshots and bigger files, at the cost of redoing more after a failed commit.
    #[arg(long, default_value_t = 1_000_000)]
    chunk_rows: usize,
    /// DataFusion pool cap in MiB. Accounting limit, not an RSS limit.
    #[arg(long, default_value_t = 8192)]
    memory_mb: usize,
    /// Where DataFusion spills the window sort.
    #[arg(long, default_value = "/tmp/crypto-backfill")]
    temp_dir: String,
    #[arg(long, default_value_t = 8)]
    target_partitions: usize,
    /// Print the physical plan and exit.
    #[arg(long)]
    explain: bool,
}

/// How a candidate row is rewritten before it reaches the target table, in the target's
/// column order.
///
/// The target declares all eleven columns required, so the guarantee has to be made here:
/// every entry yields a non-NULL value, and `assert_no_nulls` checks a plan that would not.
///
/// For the two flags the default *is* the semantics: they are optional in the historical
/// table only because the pre-cutover writers stored proto3's absent `false` as NULL while
/// the batcher stores `false`, so NULL means exactly `false` and coalescing keeps one
/// encoding instead of mixing two in one table. For the other nine the default is a backstop
/// that cannot fire, because [`candidate_ctes`] drops every candidate missing a
/// [`MUST_NOT_BE_NULL`] column and counts it in `rows_skipped_null`: a missing price means a
/// skipped row, never a price of 0.
///
/// Nullability comes from `coalesce(col, …)` on a *bare* column: DataFusion 53.1 rejects
/// nullability inside a cast target type (`CAST(col AS T NOT NULL)` is a `ParserError`), and
/// `WHERE col IS NOT NULL` does not narrow a plan's schema either. The two flags are the
/// exception because DataFusion keeps a coalesced `BOOLEAN` nullable — `CASE`, `IS TRUE` and
/// `IS NOT DISTINCT FROM` behave the same — so only they detour through `INT`
/// (`NULL → 0 → false`) and cast back.
///
/// Bare arguments also keep the projection in one piece: an argument that is itself an
/// expression gets hoisted into a common subexpression, which is what reorders a projection's
/// inputs. Nothing here depends on that any more, but it is worth not fighting it.
///
/// Both tables hold identical epoch micros, and reading the historical `timestamp` through
/// iceberg already yields `Timestamp(µs, "+00:00")`. The explicit `arrow_cast` is a no-op
/// then and a plain reinterpretation otherwise: the destination schema is matched exactly, so
/// pinning the zone beats depending on how one library version maps a zone-less column.
const CANDIDATE_PROJECTION: &str = r#"coalesce(event_time, 0) AS event_time,
    coalesce(symbol, '') AS symbol,
    coalesce(exchange, '') AS exchange,
    coalesce(trade_id, 0) AS trade_id,
    coalesce(price, 0) AS price,
    coalesce(quantity, 0) AS quantity,
    coalesce(trade_time, 0) AS trade_time,
    CAST(coalesce(CAST(is_buyer_maker AS INT), 0) AS BOOLEAN) AS is_buyer_maker,
    CAST(coalesce(CAST(is_best_price_match AS INT), 0) AS BOOLEAN) AS is_best_price_match,
    arrow_cast(coalesce(trade_ts, TIMESTAMP '1970-01-01 00:00:00'),
               'Timestamp(Microsecond, Some("+00:00"))') AS trade_ts,
    arrow_cast(coalesce(event_ts, TIMESTAMP '1970-01-01 00:00:00'),
               'Timestamp(Microsecond, Some("+00:00"))') AS event_ts"#;

/// Candidate columns the target requires and that have no honest default, so a candidate
/// missing one is left out of the append instead of being rewritten. The two booleans are
/// absent on purpose: NULL there means proto3-false, see [`CANDIDATE_PROJECTION`].
const MUST_NOT_BE_NULL: [&str; 9] = [
    "event_time",
    "symbol",
    "exchange",
    "trade_id",
    "price",
    "quantity",
    "trade_time",
    "trade_ts",
    "event_ts",
];

/// The `src_ranked` / `target_keys` / `missing` CTEs shared by the audit and the append.
///
/// `source` and `target` are fully qualified (`lk.trades.trades`) so the tests can point
/// them at `MemTable`s without dragging in a catalog.
///
/// `missing` is the append list, so it applies both admission rules: one row per
/// `(exchange, symbol, trade_id)` key, and only candidates complete enough for the target
/// (`complete`, checked by [`MUST_NOT_BE_NULL`]).
fn candidate_ctes(source: &str, target: &str, start: &str, end: &str, margin_days: i64) -> String {
    let complete_predicate = MUST_NOT_BE_NULL
        .iter()
        .map(|column| format!("{column} IS NOT NULL"))
        .collect::<Vec<_>>()
        .join(" AND ");
    let mut sql = String::new();
    writeln!(
        sql,
        r#"WITH src_ranked AS (
               SELECT event_time, symbol, exchange, trade_id, price, quantity, trade_time,
                      is_buyer_maker, is_best_price_match, trade_ts, event_ts,
                      is_buyer_maker IS NULL AS maker_was_null,
                      ({complete_predicate}) AS complete,
                      row_number() OVER (
                          PARTITION BY exchange, symbol, trade_id
                          ORDER BY event_ts, trade_ts
                      ) AS rn
               FROM {source}
               WHERE trade_ts >= TIMESTAMP '{start}' AND trade_ts < TIMESTAMP '{end}'
           ),
           target_keys AS (
               SELECT DISTINCT exchange, symbol, trade_id
               FROM {target}
               WHERE trade_ts >= TIMESTAMP '{start}' - INTERVAL '{margin_days} days'
                 AND trade_ts <  TIMESTAMP '{end}'   + INTERVAL '{margin_days} days'
           ),
           missing AS (
               SELECT s.event_time, s.symbol, s.exchange, s.trade_id, s.price, s.quantity,
                      s.trade_time, s.is_buyer_maker, s.is_best_price_match, s.trade_ts,
                      s.event_ts
               FROM src_ranked s
               LEFT ANTI JOIN target_keys t
                    ON t.exchange = s.exchange
                   AND t.symbol   = s.symbol
                   AND t.trade_id = s.trade_id
               WHERE s.rn = 1
                 AND s.complete
           )"#
    )
    .expect("writing to a String cannot fail");
    sql
}

/// One row of numbers: how full the window is, how much of it the target already has.
///
/// The join back to `missing` is deliberately not restricted to the first occurrence, so a
/// duplicated trade matches its candidate row several times; every metric that must not
/// count redelivery repeats therefore filters on `rn = 1`.
///
/// `rows_to_append` is exactly what `--execute` writes. `rows_skipped_null` counts the first
/// occurrence of every key the append refuses for missing data, whether or not the target
/// happens to hold it already — `complete` decides on its own there.
fn audit_sql(ctes: &str) -> String {
    format!(
        r#"{ctes}
           SELECT count(*)                                              AS rows_in_window,
                  count(*) FILTER (WHERE s.rn = 1)                       AS distinct_keys,
                  count(*) FILTER (WHERE s.rn > 1)                       AS duplicate_rows,
                  count(*) FILTER (WHERE s.maker_was_null AND s.rn = 1)  AS maker_nulls,
                  count(*) FILTER (WHERE s.rn = 1 AND NOT s.complete)    AS rows_skipped_null,
                  count(*) FILTER (WHERE s.rn = 1 AND m.trade_id IS NOT NULL) AS rows_to_append
           FROM src_ranked s
           LEFT JOIN missing m
                ON m.exchange = s.exchange
               AND m.symbol   = s.symbol
               AND m.trade_id = s.trade_id
        "#
    )
}

/// The rows to append, in the destination's column order and types: what
/// [`IcebergSink::append_records`] is handed, one chunk at a time.
fn candidate_sql(ctes: &str) -> String {
    format!("{ctes} SELECT {CANDIDATE_PROJECTION} FROM missing")
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    tracing_subscriber::fmt::init();

    let from = checked_qualified_name(&args.from)?.to_string();
    let into = checked_qualified_name(&args.into)?.to_string();

    let ctx = iceberg_session(&EngineOptions {
        memory_mb: args.memory_mb,
        temp_dir: Some(args.temp_dir.clone().into()),
        target_partitions: args.target_partitions,
        ..EngineOptions::default()
    })
    .await?;

    let source = format!("{CATALOG}.{from}");
    let target = format!("{CATALOG}.{into}");
    let ctes = candidate_ctes(
        &source,
        &target,
        &args.start,
        &args.end,
        args.target_margin_days,
    );

    eprintln!(
        "[info] {from} -> {into}, trade_ts [{}, {}), target margin {} day(s), pool {} MiB{}",
        args.start,
        args.end,
        args.target_margin_days,
        args.memory_mb,
        if args.execute { "" } else { " (DRY RUN)" }
    );

    if args.execute {
        if args.explain {
            return run(&ctx, &candidate_sql(&ctes), true, |_| Ok(())).await;
        }
        let start = Instant::now();
        let mut sink = append::open(&into).await?;
        let appended =
            append::stream_into(&ctx, &mut sink, &candidate_sql(&ctes), args.chunk_rows).await?;
        println!("appended  : {appended} rows to {target}");
        let visible =
            append::ensure_visible(&ctx, &target, &args.start, &args.end, appended).await?;
        println!("visible   : {visible} rows readable in the window");
        println!("elapsed   : {:?}", start.elapsed());
        println!("peak rss  : {} MiB", peak_rss_mib());
        return Ok(());
    }

    run(&ctx, &audit_sql(&ctes), args.explain, |batches| {
        let batch = batches.first().context("audit returned no rows")?;
        for column in [
            "rows_in_window",
            "distinct_keys",
            "duplicate_rows",
            "maker_nulls",
            "rows_skipped_null",
            "rows_to_append",
        ] {
            println!("{column:>15}: {}", count_from(batch, column)?);
        }
        Ok(())
    })
    .await
}

async fn run<F>(
    ctx: &datafusion::prelude::SessionContext,
    sql: &str,
    explain: bool,
    report: F,
) -> Result<()>
where
    F: FnOnce(&[RecordBatch]) -> Result<()>,
{
    let df = ctx
        .sql(sql)
        .await
        .context("failed to plan backfill query")?;
    if explain {
        df.clone().explain(false, true)?.show().await?;
        return Ok(());
    }
    let start = Instant::now();
    let batches = df.collect().await.context("failed to run backfill query")?;
    report(&batches)?;
    println!("elapsed   : {:?}", start.elapsed());
    println!("peak rss  : {} MiB", peak_rss_mib());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crypto_iceberg_query::TRADE_COLUMNS;
    use datafusion::arrow::array::{
        Array, BooleanArray, Float64Array, Int64Array, StringArray, TimestampMicrosecondArray,
        new_null_array,
    };
    use datafusion::arrow::datatypes::{DataType, Field, Schema, TimeUnit};
    use datafusion::datasource::MemTable;
    use datafusion::prelude::SessionContext;
    use std::sync::Arc;

    /// Days since epoch -> micros, so fixtures line up with the SQL literals
    /// (day 0 = 1970-01-01, which is what the range filter compares against).
    fn ts(day: i64, secs: i64) -> i64 {
        (day * 86_400 + secs) * 1_000_000
    }

    /// The pre-cutover spelling: every column optional, `trade_ts`/`event_ts` without a
    /// zone. `maker` is `None` wherever the flag was proto3-false.
    fn source_schema() -> Arc<Schema> {
        Arc::new(Schema::new(vec![
            Field::new("event_time", DataType::Int64, true),
            Field::new("symbol", DataType::Utf8, true),
            Field::new("exchange", DataType::Utf8, true),
            Field::new("trade_id", DataType::Int64, true),
            Field::new("price", DataType::Float64, true),
            Field::new("quantity", DataType::Float64, true),
            Field::new("trade_time", DataType::Int64, true),
            Field::new("is_buyer_maker", DataType::Boolean, true),
            Field::new("is_best_price_match", DataType::Boolean, true),
            Field::new(
                "trade_ts",
                DataType::Timestamp(TimeUnit::Microsecond, None),
                true,
            ),
            Field::new(
                "event_ts",
                DataType::Timestamp(TimeUnit::Microsecond, None),
                true,
            ),
        ]))
    }

    /// The batcher's spelling: zoned timestamps, and only the key + partition column are
    /// needed to answer "already present?".
    fn target_schema() -> Arc<Schema> {
        let tz: Arc<str> = Arc::from("+00:00");
        Arc::new(Schema::new(vec![
            Field::new("exchange", DataType::Utf8, false),
            Field::new("symbol", DataType::Utf8, false),
            Field::new("trade_id", DataType::Int64, false),
            Field::new(
                "trade_ts",
                DataType::Timestamp(TimeUnit::Microsecond, Some(tz)),
                false,
            ),
        ]))
    }

    /// One row of the source table: `(trade_id, symbol, maker, trade_ts micros)`.
    fn source_batch(rows: &[(i64, &str, Option<bool>, i64)]) -> RecordBatch {
        RecordBatch::try_new(
            source_schema(),
            vec![
                Arc::new(Int64Array::from(
                    rows.iter().map(|r| r.3 / 1000).collect::<Vec<_>>(),
                )),
                Arc::new(StringArray::from(
                    rows.iter().map(|r| r.1).collect::<Vec<_>>(),
                )),
                Arc::new(StringArray::from(vec!["BINANCE"; rows.len()])),
                Arc::new(Int64Array::from(
                    rows.iter().map(|r| r.0).collect::<Vec<_>>(),
                )),
                Arc::new(Float64Array::from(vec![100.0; rows.len()])),
                Arc::new(Float64Array::from(vec![1.0; rows.len()])),
                Arc::new(Int64Array::from(
                    rows.iter().map(|r| r.3 / 1000).collect::<Vec<_>>(),
                )),
                Arc::new(BooleanArray::from(
                    rows.iter().map(|r| r.2).collect::<Vec<_>>(),
                )),
                Arc::new(BooleanArray::from(vec![Some(true); rows.len()])),
                Arc::new(TimestampMicrosecondArray::from(
                    rows.iter().map(|r| Some(r.3)).collect::<Vec<_>>(),
                )),
                Arc::new(TimestampMicrosecondArray::from(
                    rows.iter().map(|r| Some(r.3)).collect::<Vec<_>>(),
                )),
            ],
        )
        .unwrap()
    }

    fn source_ctx(rows: &[(i64, &str, Option<bool>, i64)]) -> SessionContext {
        source_ctx_from(source_batch(rows))
    }

    /// The same rows with every value of `column` set to NULL, standing for the holes the
    /// historical table has.
    fn source_ctx_nulling(rows: &[(i64, &str, Option<bool>, i64)], column: &str) -> SessionContext {
        source_ctx_from(null_out(source_batch(rows), column))
    }

    fn source_ctx_from(batch: RecordBatch) -> SessionContext {
        let ctx = SessionContext::new();
        ctx.register_table(
            "src",
            Arc::new(MemTable::try_new(source_schema(), vec![vec![batch]]).unwrap()),
        )
        .unwrap();
        ctx
    }

    /// One column of `batch`, all NULL. Panics on an unknown name, so a typo in a test
    /// cannot quietly test nothing.
    fn null_out(batch: RecordBatch, column: &str) -> RecordBatch {
        let schema = batch.schema();
        let columns = schema
            .fields()
            .iter()
            .enumerate()
            .map(|(index, field)| {
                if field.name() == column {
                    new_null_array(field.data_type(), batch.num_rows())
                } else {
                    batch.column(index).clone()
                }
            })
            .collect::<Vec<_>>();
        assert!(
            schema.fields().iter().any(|f| f.name() == column),
            "no `{column}` in the source fixture"
        );
        RecordBatch::try_new(schema, columns).unwrap()
    }

    fn register_target(ctx: &SessionContext, ids: &[i64], day: i64) {
        let batch = RecordBatch::try_new(
            target_schema(),
            vec![
                Arc::new(StringArray::from(vec!["BINANCE"; ids.len()])),
                Arc::new(StringArray::from(vec!["BTCUSDT"; ids.len()])),
                Arc::new(Int64Array::from(ids.to_vec())),
                Arc::new(
                    TimestampMicrosecondArray::from(
                        ids.iter().map(|_| Some(ts(day, 0))).collect::<Vec<_>>(),
                    )
                    .with_data_type(DataType::Timestamp(
                        TimeUnit::Microsecond,
                        Some(Arc::from("+00:00")),
                    )),
                ),
            ],
        )
        .unwrap();
        ctx.register_table(
            "tgt",
            Arc::new(MemTable::try_new(target_schema(), vec![vec![batch]]).unwrap()),
        )
        .unwrap();
    }

    /// Every fixture row lives on day 2 unless the test says otherwise, so one window
    /// description covers them all.
    const WINDOW: (&str, &str) = ("1970-01-02 00:00:00", "1970-01-04 00:00:00");

    async fn audit(ctx: &SessionContext) -> Vec<(String, u64)> {
        let ctes = candidate_ctes("src", "tgt", WINDOW.0, WINDOW.1, 1);
        let df = ctx.sql(&audit_sql(&ctes)).await.unwrap();
        let batches = df.collect().await.unwrap();
        let batch = batches.first().unwrap();
        [
            "rows_in_window",
            "distinct_keys",
            "duplicate_rows",
            "maker_nulls",
            "rows_skipped_null",
            "rows_to_append",
        ]
        .iter()
        .map(|c| ((*c).to_string(), count_from(batch, c).unwrap()))
        .collect()
    }

    /// A source row inside the window survives, one outside it does not, and a row the
    /// target already has is never appended twice.
    #[tokio::test]
    async fn anti_join_keeps_only_missing_rows() {
        let day = 2;
        let ctx = source_ctx(&[
            (10, "BTCUSDT", Some(true), ts(day, 10)), // already in target
            (20, "BTCUSDT", None, ts(day, 20)),       // missing, maker NULL
            (99, "BTCUSDT", Some(false), ts(day + 5, 0)), // out of window
        ]);
        register_target(&ctx, &[10], day);
        let got = audit(&ctx).await;
        let get = |name: &str| got.iter().find(|(k, _)| k == name).unwrap().1;
        assert_eq!(
            get("rows_in_window"),
            2,
            "window holds the two in-window rows"
        );
        assert_eq!(get("distinct_keys"), 2);
        assert_eq!(get("duplicate_rows"), 0);
        assert_eq!(get("maker_nulls"), 1);
        assert_eq!(get("rows_to_append"), 1, "only trade 20 is missing");
    }

    /// Redelivered rows inside the window collapse to one candidate per trade key.
    #[tokio::test]
    async fn duplicate_source_rows_are_collapsed() {
        let day = 2;
        let ctx = source_ctx(&[
            (20, "BTCUSDT", None, ts(day, 20)),
            (20, "BTCUSDT", None, ts(day, 21)),
            (20, "BTCUSDT", None, ts(day, 22)),
            (30, "BTCUSDT", Some(true), ts(day, 30)),
        ]);
        register_target(&ctx, &[], day);
        let got = audit(&ctx).await;
        let get = |name: &str| got.iter().find(|(k, _)| k == name).unwrap().1;
        assert_eq!(get("rows_in_window"), 4);
        assert_eq!(get("duplicate_rows"), 2);
        assert_eq!(get("rows_to_append"), 2);
    }

    /// Nothing to do when the target already holds every key in the window: the audit still
    /// reports the window, and zero appends.
    #[tokio::test]
    async fn fully_present_window_appends_nothing() {
        let day = 2;
        let ctx = source_ctx(&[
            (10, "BTCUSDT", Some(true), ts(day, 10)),
            (11, "BTCUSDT", None, ts(day, 11)),
        ]);
        register_target(&ctx, &[10, 11], day);
        let got = audit(&ctx).await;
        assert_eq!(got.last().unwrap().1, 0, "rows_to_append");
    }

    /// A candidate missing a column the target requires cannot be written there, so it is
    /// left out of the append and reported — not quietly given a price of 0.
    #[tokio::test]
    async fn null_in_a_required_column_is_skipped_and_reported() {
        let day = 2;
        let rows = [
            (20, "BTCUSDT", None, ts(day, 20)),
            (21, "BTCUSDT", Some(true), ts(day, 21)),
        ];
        let ctx = source_ctx_nulling(&rows, "price");
        register_target(&ctx, &[], day);
        let got = audit(&ctx).await;
        let get = |name: &str| got.iter().find(|(k, _)| k == name).unwrap().1;
        assert_eq!(get("rows_in_window"), 2, "both rows are in the window");
        assert_eq!(get("rows_skipped_null"), 2, "neither of them has a price");
        assert_eq!(get("rows_to_append"), 0, "so neither is appended");
    }

    /// The two flags are the exception to that rule: proto3-absent means `false`, so a NULL
    /// flag is a row to backfill rather than a row to skip.
    #[tokio::test]
    async fn null_flags_are_backfilled_rather_than_skipped() {
        let day = 2;
        let ctx = source_ctx_nulling(&[(20, "BTCUSDT", None, ts(day, 20))], "is_best_price_match");
        register_target(&ctx, &[], day);
        let got = audit(&ctx).await;
        let get = |name: &str| got.iter().find(|(k, _)| k == name).unwrap().1;
        assert_eq!(
            get("rows_skipped_null"),
            0,
            "no flag is a required-value gap"
        );
        assert_eq!(get("rows_to_append"), 1, "the row itself is still whole");
    }

    /// Nothing may be written NULL into the target's required columns, and nothing checks it
    /// for us, so check the batch the append would hand to Iceberg.
    fn assert_no_nulls(batch: &RecordBatch) {
        let schema = batch.schema();
        for (index, column) in batch.columns().iter().enumerate() {
            let nulls = (0..column.len()).filter(|row| column.is_null(*row)).count();
            assert_eq!(
                nulls,
                0,
                "`{}` would be written NULL into a required column",
                schema.field(index).name()
            );
        }
    }

    /// The append projection is what the target's schema asks for: `TRADE_COLUMNS` order,
    /// proto3-false restored on both flags, and the same epoch micros under the zoned type.
    #[tokio::test]
    async fn projection_matches_the_target_contract() {
        let day = 2;
        let micros = ts(day, 20);
        let ctx = source_ctx_nulling(&[(20, "BTCUSDT", None, micros)], "is_best_price_match");
        register_target(&ctx, &[], day);
        let ctes = candidate_ctes("src", "tgt", WINDOW.0, WINDOW.1, 1);
        let df = ctx
            .sql(&format!(
                "{ctes} SELECT {CANDIDATE_PROJECTION} FROM missing"
            ))
            .await
            .unwrap();
        let batch = df.collect().await.unwrap().remove(0);

        assert_eq!(
            TRADE_COLUMNS.split(", ").collect::<Vec<_>>(),
            batch
                .schema()
                .fields()
                .iter()
                .map(|f| f.name().as_str())
                .collect::<Vec<_>>(),
            "append projection must be in the target's column order"
        );

        assert_no_nulls(&batch);

        let schema = batch.schema();
        for field in schema.fields() {
            assert!(
                !field.is_nullable(),
                "`{}` is nullable but the destination requires every column, and \
                 `append::stream_into` refuses a batch that is not its schema",
                field.name()
            );
        }

        // Type equality is part of the same contract, and refusing the write beats writing
        // files the partition transforms later misread. `Utf8View` is what DataFusion likes to
        // produce.
        let target_types = [
            ("event_time", DataType::Int64),
            ("symbol", DataType::Utf8),
            ("exchange", DataType::Utf8),
            ("trade_id", DataType::Int64),
            ("price", DataType::Float64),
            ("quantity", DataType::Float64),
            ("trade_time", DataType::Int64),
            ("is_buyer_maker", DataType::Boolean),
            ("is_best_price_match", DataType::Boolean),
            (
                "trade_ts",
                DataType::Timestamp(TimeUnit::Microsecond, Some(Arc::from("+00:00"))),
            ),
            (
                "event_ts",
                DataType::Timestamp(TimeUnit::Microsecond, Some(Arc::from("+00:00"))),
            ),
        ];
        for (name, expected) in target_types {
            assert_eq!(
                schema.field_with_name(name).unwrap().data_type(),
                &expected,
                "`{name}` must arrive as the target's own type",
            );
        }

        let flag = |name: &str| {
            batch
                .column_by_name(name)
                .unwrap()
                .as_any()
                .downcast_ref::<BooleanArray>()
                .unwrap()
                .value(0)
        };
        assert!(!flag("is_buyer_maker"), "proto3-absent means false");
        assert!(!flag("is_best_price_match"), "same for the other flag");

        for name in ["trade_ts", "event_ts"] {
            let stamped = batch
                .column_by_name(name)
                .unwrap()
                .as_any()
                .downcast_ref::<TimestampMicrosecondArray>()
                .unwrap();
            assert_eq!(
                stamped.data_type(),
                &DataType::Timestamp(TimeUnit::Microsecond, Some(Arc::from("+00:00"))),
                "{name} must land as the target's zoned type"
            );
            assert_eq!(
                stamped.value(0),
                micros,
                "epoch micros must survive the cast"
            );
        }
    }

    /// …and the projection's nine backstop defaults stay unreachable: a candidate missing a
    /// required column is dropped before it, rather than appended with a made-up value.
    #[tokio::test]
    async fn incomplete_candidates_are_never_projected() {
        let day = 2;
        let ctx = source_ctx_nulling(&[(20, "BTCUSDT", None, ts(day, 20))], "price");
        register_target(&ctx, &[], day);
        let ctes = candidate_ctes("src", "tgt", WINDOW.0, WINDOW.1, 1);
        let df = ctx
            .sql(&format!(
                "{ctes} SELECT {CANDIDATE_PROJECTION} FROM missing"
            ))
            .await
            .unwrap();
        let rows = df
            .collect()
            .await
            .unwrap()
            .iter()
            .map(RecordBatch::num_rows)
            .sum::<usize>();
        assert_eq!(rows, 0, "skipped, not appended with a defaulted price");
    }

    /// The clause builder is the only place the window is spelled, and a zero margin must
    /// still produce a valid (if pointless) filter.
    #[test]
    fn ctes_reference_both_tables_and_both_bounds() {
        let sql = candidate_ctes(
            "lk.a.b",
            "lk.c.d",
            "2026-10-06 19:00:00",
            "2026-10-06 20:00:00",
            0,
        );
        assert!(sql.contains("FROM lk.a.b"));
        assert!(sql.contains("FROM lk.c.d"));
        assert!(sql.contains("TIMESTAMP '2026-10-06 19:00:00'"));
        assert!(sql.contains("TIMESTAMP '2026-10-06 20:00:00'"));
        assert!(sql.contains("LEFT ANTI JOIN"));
        let candidates = candidate_sql(&sql);
        assert!(
            candidates.starts_with("WITH src_ranked"),
            "no INSERT any more: the candidates are streamed to the sink"
        );
        assert!(candidates.ends_with("FROM missing"));
        assert!(candidates.contains("AS is_buyer_maker"));
    }
}
