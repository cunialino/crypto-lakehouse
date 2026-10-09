//! Per-day row counts for one `trades` table over a `trade_ts` range — the quick
//! "did the writer keep up / are the days contiguous" check.
//!
//! ```text
//! crypto-iceberg-query '2026-10-01 00:00:00' '2026-10-08 00:00:00'
//! ```
//!
//! Catalog + storage config: see `crypto_iceberg_query::iceberg_session`.

use anyhow::{Context, Result};
use clap::Parser;
use crypto_iceberg_query::{EngineOptions, iceberg_session, peak_rss_mib};
use std::time::Instant;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    start_date: String,
    end_date: String,
    /// Table to count, as `<namespace>.<table>`.
    #[arg(long, default_value = "trades.trades")]
    table: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    tracing_subscriber::fmt::init();

    let table = crypto_iceberg_query::checked_qualified_name(&args.table)?;

    // Historically this ran with a 16 GiB pool because `count(DISTINCT concat(...))` over
    // ~215M distinct keys peaks near 30 GiB and DataFusion 53.1 does not account that
    // final-merge hash table against the pool (see `iceberg_session`). The pool is a hint
    // for that operator, so the run prints real peak RSS afterwards.
    let ctx = iceberg_session(&EngineOptions {
        memory_mb: 16 * 1024,
        ..EngineOptions::default()
    })
    .await?;
    eprintln!("[progress] catalog registered");

    let day_sql = format!(
        r#"
        SELECT
            to_char(trade_ts, '%Y%m%d') AS day,
            count(*) AS cnt,
            count(DISTINCT concat(exchange, cast(trade_id as varchar), symbol)) AS cnt_dist
        FROM lk.{table}
        WHERE trade_ts between TIMESTAMP '{}' AND TIMESTAMP '{}'
        GROUP BY day
        ORDER BY day
    "#,
        args.start_date, args.end_date
    );

    let start = Instant::now();
    let df = ctx.sql(&day_sql).await.context("run day query")?;
    eprintln!("[progress] day query planned, starting show");
    df.show().await.context("show day query")?;
    println!("day-agg elapsed: {:?}", start.elapsed());
    println!("peak rss: {} MiB", peak_rss_mib());

    Ok(())
}
