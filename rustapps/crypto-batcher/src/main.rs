//! `crypto-batcher` — NATS JetStream → Iceberg micro-batcher.
//!
//! Replaces the Arroyo `arroyo-lakehouse` pipeline (`deploy/base/arroyo/sql/lakehouse.sql`)
//! with a single Rust binary:
//!
//! ```text
//! NATS JetStream tradesstream (exchange.*)
//!   └─ durable pull consumer (explicit ack)      ← the pipeline offset lives here
//!        └─ decode trade.data.TradeEventProto
//!             └─ Accumulator: rows / bytes / age  ← the micro-batch
//!                  └─ Iceberg fast_append commit  ← one snapshot per micro-batch
//!                       └─ ack messages           ← at-least-once
//! ```
//!
//! Run `--ensure-only` to create the destination table and exit (useful as an init
//! container or a smoke test).

use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::sync::watch;
use tracing_subscriber::EnvFilter;

use crypto_batcher::batcher;
use crypto_batcher::config::Config;
use crypto_batcher::sink::IcebergSink;
use crypto_batcher::source;

#[derive(Parser, Debug)]
#[command(
    version,
    about = "Micro-batch NATS trade events into an Iceberg table (no stream processor required)"
)]
struct Args {
    /// Connect, ensure the destination table exists, print it and exit.
    #[arg(long)]
    ensure_only: bool,

    /// Log level (debug, info, warn). Overridden by RUST_LOG when set.
    #[arg(short, long, default_value = "info")]
    log_level: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    init_tracing(&args.log_level);

    let cfg = Config::from_env().context("invalid configuration")?;
    tracing::info!(
        table = format!("{}.{}", cfg.iceberg.namespace, cfg.iceberg.table),
        stream = %cfg.nats.stream,
        consumer = %cfg.nats.consumer,
        max_records = cfg.batch.max_records,
        max_bytes = cfg.batch.max_bytes,
        flush_interval_ms = cfg.batch.flush_interval.as_millis() as u64,
        dedup = cfg.batch.dedup,
        "starting crypto-batcher"
    );

    let mut sink = IcebergSink::connect(cfg.iceberg.clone()).await?;
    if args.ensure_only {
        println!(
            "table {}.{} ready at {}",
            cfg.iceberg.namespace,
            cfg.iceberg.table,
            sink.table().metadata().location()
        );
        return Ok(());
    }

    let trades = source::spawn_nats_source(&cfg.nats, &cfg.batch)
        .await
        .context("failed to start nats source")?;

    let (shutdown_tx, mut shutdown_rx) = watch::channel(());
    spawn_signal_handler(shutdown_tx);

    let stats = batcher::run(trades, &mut sink, cfg.batch.clone(), async move {
        // Fires once on SIGTERM/SIGINT; `run` then flushes the remainder and returns.
        let _ = shutdown_rx.changed().await;
    })
    .await?;

    tracing::info!(
        batches = stats.batches,
        rows = stats.rows,
        files = stats.files,
        bytes = stats.bytes,
        duplicates_dropped = stats.duplicates_dropped,
        failed_commits = stats.failed_commits,
        ack_failures = stats.ack_failures,
        "crypto-batcher stopped"
    );
    Ok(())
}

fn init_tracing(level: &str) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .init();
}

/// Forwards SIGINT/SIGTERM into the watch channel so the batch loop can flush first.
fn spawn_signal_handler(tx: watch::Sender<()>) {
    tokio::spawn(async move {
        let terminate = shutdown_signal(tokio::signal::unix::SignalKind::terminate());
        let interrupt = shutdown_signal(tokio::signal::unix::SignalKind::interrupt());
        tokio::select! {
            _ = terminate => tracing::info!("SIGTERM received; flushing and stopping"),
            _ = interrupt => tracing::info!("SIGINT received; flushing and stopping"),
        }
        // Receiver may already be gone if `run` returned first (source closed).
        let _ = tx.send(());
    });
}

async fn shutdown_signal(kind: tokio::signal::unix::SignalKind) {
    match tokio::signal::unix::signal(kind) {
        Ok(mut stream) => {
            stream.recv().await;
        }
        // No signal support (unusual platform): effectively never fire, rely on the source
        // closing. (tokio's `sleep` rejects `Duration::MAX`, hence one year.)
        Err(_) => tokio::time::sleep(Duration::from_secs(365 * 24 * 3600)).await,
    }
}
