//! End-to-end tests of the micro-batching pipeline against a real Iceberg table, using
//! iceberg's in-memory catalog + in-memory `FileIO`, so they run anywhere with no
//! infrastructure. They cover everything except the NATS and S3 wire adapters:
//!
//! - rows reach the table through the flush loop, one snapshot per micro-batch,
//! - committed data is readable back through an Iceberg scan, with the right values and
//!   the `exchange=<x>/trade_ts_day=<date>` layout on disk,
//! - duplicates are dropped but still acknowledged,
//! - a failed commit leaves messages unacked (at-least-once, no silent data loss).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use arrow_array::{Array, Int64Array, RecordBatch, StringArray, TimestampMicrosecondArray};
use futures::TryStreamExt;
use iceberg::CatalogBuilder;
use iceberg::memory::{MEMORY_CATALOG_WAREHOUSE, MemoryCatalogBuilder};
use iceberg::{Catalog, NamespaceIdent, TableIdent};
use tokio::sync::watch;

use crypto_batcher::batcher::{self, Stats};
use crypto_batcher::config::{BatchConfig, IcebergConfig};
use crypto_batcher::schema::TradeEvent;
use crypto_batcher::sink::IcebergSink;
use crypto_batcher::source::{self, Ack, Envelope};

const DAY_MS: u64 = 24 * 60 * 60 * 1000;
const MICROS_PER_DAY: i64 = 24 * 60 * 60 * 1_000_000;
/// 2026-08-07T00:00:00Z, so `day(trade_ts)` produces two distinct epoch days.
const BASE_MS: u64 = 1_786_060_800_000;

fn table_ident() -> TableIdent {
    TableIdent::new(NamespaceIdent::new("trades".into()), "trades_mem".into())
}

fn iceberg_config() -> IcebergConfig {
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

async fn memory_sink() -> (Arc<dyn Catalog>, IcebergSink) {
    let catalog: Arc<dyn Catalog> = Arc::new(
        MemoryCatalogBuilder::default()
            .load(
                "memory",
                HashMap::from([(
                    MEMORY_CATALOG_WAREHOUSE.to_string(),
                    "memory://wh".to_string(),
                )]),
            )
            .await
            .expect("memory catalog"),
    );
    let sink = IcebergSink::with_catalog(catalog.clone(), iceberg_config())
        .await
        .expect("sink auto-creates the table");
    (catalog, sink)
}

fn trade(id: u64, exchange: &str, symbol: &str, day_offset: u64) -> Envelope {
    Envelope {
        event: TradeEvent {
            event_time: BASE_MS + day_offset * DAY_MS + id,
            symbol: symbol.into(),
            exchange: exchange.into(),
            trade_id: id,
            price: 100.0 + id as f64,
            quantity: 0.5,
            trade_time: BASE_MS + day_offset * DAY_MS + id,
            is_buyer_maker: id.is_multiple_of(2),
            is_best_price_match: true,
        },
        bytes: 64,
        ack: Ack::None,
    }
}

/// Starts the batch loop in the background with an external shutdown handle.
fn spawn_batcher(
    mut sink: IcebergSink,
    policy: BatchConfig,
    rx: source::TradeReceiver,
    mut shutdown_rx: watch::Receiver<()>,
) -> tokio::task::JoinHandle<Stats> {
    tokio::spawn(async move {
        batcher::run(rx, &mut sink, policy, async move {
            let _ = shutdown_rx.changed().await;
        })
        .await
        .expect("batch loop never errors")
    })
}

async fn scan_all(table: &iceberg::table::Table) -> Vec<RecordBatch> {
    table
        .scan()
        .build()
        .unwrap()
        .to_arrow()
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap()
}

fn string_column(batches: &[RecordBatch], name: &str) -> Vec<String> {
    batches
        .iter()
        .flat_map(|b| {
            let col = b
                .column_by_name(name)
                .unwrap()
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap();
            (0..col.len()).map(|i| col.value(i).to_string())
        })
        .collect()
}

fn i64_column(batches: &[RecordBatch], name: &str) -> Vec<i64> {
    batches
        .iter()
        .flat_map(|b| {
            let col = b
                .column_by_name(name)
                .unwrap()
                .as_any()
                .downcast_ref::<Int64Array>()
                .unwrap();
            (0..col.len()).map(|i| col.value(i))
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn micro_batches_reach_the_table_and_stay_readable() {
    let (catalog, sink) = memory_sink().await;
    let policy = BatchConfig {
        max_records: 10,
        max_bytes: 10 * 1024 * 1024,
        flush_interval: Duration::from_millis(40),
        dedup: true,
    };

    let (tx, rx) = source::trade_channel(64);
    let (shutdown_tx, shutdown_rx) = watch::channel(());
    let handle = spawn_batcher(sink, policy, rx, shutdown_rx);

    // 30 rows over 2 exchanges x 2 days (10 BTC + 5 ETH per day). Trade ids are unique per
    // (exchange, symbol),
    // exactly like a real exchange stream - and exactly the key the Spark dedup job uses
    // (`concat(exchange, trade_id, symbol)`).
    for day in 0..2u64 {
        for i in 0..10u64 {
            tx.send(trade(day * 100 + i, "BINANCE", "BTCUSDT", day))
                .await
                .unwrap();
        }
        for i in 0..5u64 {
            tx.send(trade(day * 100 + i, "OKX", "ETHUSDT", day))
                .await
                .unwrap();
        }
    }

    tokio::time::sleep(Duration::from_millis(400)).await;
    shutdown_tx.send(()).unwrap();
    let stats: Stats = handle.await.unwrap();

    assert_eq!(stats.rows, 30, "all rows committed: {stats:?}");
    assert!(
        stats.batches >= 2,
        "count trigger + shutdown drain => {stats:?}"
    );
    assert_eq!(stats.duplicates_dropped, 0);
    assert_eq!(stats.failed_commits, 0);

    let table = catalog.load_table(&table_ident()).await.unwrap();
    assert_eq!(
        table.metadata().snapshots().count(),
        stats.batches as usize,
        "exactly one snapshot per micro-batch"
    );

    let batches = scan_all(&table).await;
    assert_eq!(
        batches.iter().map(|b| b.num_rows()).sum::<usize>(),
        30,
        "every row is readable after commit"
    );

    let exchanges: HashSet<String> = string_column(&batches, "exchange").into_iter().collect();
    assert_eq!(exchanges, HashSet::from(["BINANCE".into(), "OKX".into()]));
    let symbols: HashSet<String> = string_column(&batches, "symbol").into_iter().collect();
    assert_eq!(symbols, HashSet::from(["BTCUSDT".into(), "ETHUSDT".into()]));

    // trade_ts must be microseconds derived from millisecond input.
    let micros: Vec<i64> = batches
        .iter()
        .flat_map(|b| {
            let col = b
                .column_by_name("trade_ts")
                .unwrap()
                .as_any()
                .downcast_ref::<TimestampMicrosecondArray>()
                .unwrap();
            (0..col.len()).map(|i| col.value(i))
        })
        .collect();
    assert!(
        micros.iter().all(|v| *v > 1_000_000_000_000_000),
        "trade_ts is micros, not millis: {micros:?}"
    );
    let days: HashSet<i64> = micros.iter().map(|v| v / MICROS_PER_DAY).collect();
    assert_eq!(days.len(), 2, "two day partitions: {days:?}");

    // On-disk layout follows the partition spec (this is what makes partition pruning work).
    let paths: Vec<String> = table
        .scan()
        .build()
        .unwrap()
        .plan_files()
        .await
        .unwrap()
        .map_ok(|task| task.data_file_path().to_string())
        .try_collect()
        .await
        .unwrap();
    assert!(!paths.is_empty());
    for path in &paths {
        assert!(
            path.contains("exchange=BINANCE") || path.contains("exchange=OKX"),
            "identity partition in path: {path}"
        );
        assert!(
            path.contains("trade_ts_day="),
            "day partition in path: {path}"
        );
    }
    let exchange_dirs: HashSet<String> = paths
        .iter()
        .map(|p| {
            p.split("exchange=")
                .nth(1)
                .unwrap()
                .split('/')
                .next()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(
        exchange_dirs,
        HashSet::from(["BINANCE".into(), "OKX".into()]),
        "files are spread over both exchanges"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn duplicates_are_dropped_but_still_acked() {
    let (catalog, sink) = memory_sink().await;
    let policy = BatchConfig {
        max_records: 1_000,
        max_bytes: 10 * 1024 * 1024,
        flush_interval: Duration::from_millis(20),
        dedup: true,
    };

    let acked = Arc::new(AtomicU64::new(0));
    let (tx, rx) = source::trade_channel(64);
    let (shutdown_tx, shutdown_rx) = watch::channel(());
    let handle = spawn_batcher(sink, policy, rx, shutdown_rx);

    // 6 deliveries of 4 distinct trades (the JetStream redelivery shape).
    for id in [1u64, 2, 3, 4, 2, 3] {
        let mut envelope = trade(id, "BINANCE", "BTCUSDT", 0);
        envelope.ack = Ack::Counter(acked.clone());
        tx.send(envelope).await.unwrap();
    }

    tokio::time::sleep(Duration::from_millis(200)).await;
    shutdown_tx.send(()).unwrap();
    let stats = handle.await.unwrap();

    assert_eq!(stats.rows, 4, "only distinct trades are written");
    assert_eq!(stats.duplicates_dropped, 2);
    assert_eq!(
        acked.load(Ordering::Relaxed),
        6,
        "even dropped duplicates are acked, otherwise they redeliver forever"
    );

    let table = catalog.load_table(&table_ident()).await.unwrap();
    assert_eq!(
        scan_all(&table)
            .await
            .iter()
            .map(|b| b.num_rows())
            .sum::<usize>(),
        4
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_commit_leaves_messages_unacked() {
    let (catalog, sink) = memory_sink().await;

    let acked = Arc::new(AtomicU64::new(0));
    let policy = BatchConfig {
        max_records: 5,
        max_bytes: 10 * 1024 * 1024,
        flush_interval: Duration::from_millis(20),
        dedup: true,
    };

    let (tx, rx) = source::trade_channel(64);
    let (shutdown_tx, shutdown_rx) = watch::channel(());
    let handle = spawn_batcher(sink, policy, rx, shutdown_rx);

    for i in 0..5u64 {
        let mut envelope = trade(i, "BINANCE", "BTCUSDT", 0);
        envelope.ack = Ack::Counter(acked.clone());
        tx.send(envelope).await.unwrap();
    }

    // Break the destination: the commit — and therefore the acks — must fail.
    catalog.drop_table(&table_ident()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    shutdown_tx.send(()).unwrap();

    let stats = handle.await.unwrap();
    assert!(stats.failed_commits > 0, "failure recorded: {stats:?}");
    assert_eq!(stats.rows, 0, "nothing was committed");
    assert_eq!(
        acked.load(Ordering::Relaxed),
        0,
        "uncommitted rows must NOT be acked; JetStream redelivers them"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn many_rows_survive_a_multi_file_micro_batch() {
    // Tiny target file size forces the rolling writer to split one micro-batch into
    // several parquet files, all of which must end up in the same snapshot.
    let (catalog, _first_sink) = memory_sink().await;
    let mut cfg = iceberg_config();
    cfg.target_file_size = 4096;
    let sink = IcebergSink::with_catalog(catalog.clone(), cfg)
        .await
        .unwrap();

    let policy = BatchConfig {
        max_records: 1_000,
        max_bytes: 10 * 1024 * 1024,
        flush_interval: Duration::from_millis(20),
        dedup: true,
    };
    let (tx, rx) = source::trade_channel(512);
    let (shutdown_tx, shutdown_rx) = watch::channel(());
    let handle = spawn_batcher(sink, policy, rx, shutdown_rx);

    for i in 0..2_000u64 {
        tx.send(trade(i, "BINANCE", "BTCUSDT", i / 1_000))
            .await
            .unwrap();
    }
    tokio::time::sleep(Duration::from_millis(600)).await;
    shutdown_tx.send(()).unwrap();
    let stats = handle.await.unwrap();

    assert_eq!(stats.rows, 2_000);
    assert!(
        stats.files > 1,
        "rolling writer split the batch: {} files",
        stats.files
    );

    let table = catalog.load_table(&table_ident()).await.unwrap();
    let batches = scan_all(&table).await;
    assert_eq!(batches.iter().map(|b| b.num_rows()).sum::<usize>(), 2_000);
    let mut ids = i64_column(&batches, "trade_id");
    ids.sort_unstable();
    assert_eq!(ids.first().copied(), Some(0));
    assert_eq!(ids.last().copied(), Some(1_999));
    assert_eq!(ids.len(), 2_000);
}
