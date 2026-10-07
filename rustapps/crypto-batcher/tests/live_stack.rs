//! Live-stack end-to-end test: real NATS JetStream + real Lakekeeper REST catalog + real
//! S3. It is `#[ignore]`d by default because it needs the containers from `teststack/`;
//! run it with:
//!
//! ```sh
//! ./teststack/run_e2e.sh          # brings the stack up and runs this test
//! ```
//!
//! What it proves on top of `memory_pipeline.rs`: the JetStream pull consumer really
//! reads `exchange.*`, the commit really lands in Lakekeeper/S3, and everything published
//! is queryable back through the catalog exactly once.

use std::time::Duration;

use arrow_array::{Array, Int64Array, StringArray};
use futures::TryStreamExt;
use tokio::sync::watch;

use crypto_batcher::batcher;
use crypto_batcher::config::Config;
use crypto_batcher::schema::TradeEvent;
use crypto_batcher::sink::IcebergSink;
use crypto_batcher::source;
use prost::Message as _;

/// Marker symbol for this run, so its rows can be counted independently of anything
/// already in the table.
fn marker() -> String {
    format!("IT{}", uuid::Uuid::new_v4().simple()).to_uppercase()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires teststack/: NATS + Lakekeeper + S3 (see deploy/docs/rust-batcher-guide.md)"]
async fn live_stack_end_to_end() {
    let cfg = Config::from_env().expect("config from env");
    let publish_count: u64 = std::env::var("E2E_ROWS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3_000);

    // ---- publish ------------------------------------------------------------------
    let symbol = marker();
    let js = async_nats::jetstream::new(
        async_nats::connect(&cfg.nats.url)
            .await
            .expect("connect nats"),
    );
    let stream = js
        .get_or_create_stream(async_nats::jetstream::stream::Config {
            name: cfg.nats.stream.clone(),
            subjects: vec!["exchange.*".into()],
            max_age: Duration::from_secs(72 * 3600),
            ..Default::default()
        })
        .await
        .expect("stream");
    stream.purge().await.ok(); // start from an empty stream for a deterministic count

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    for i in 0..publish_count {
        let event = TradeEvent {
            event_time: now_ms,
            symbol: symbol.clone(),
            exchange: if i % 2 == 0 { "BINANCE" } else { "OKX" }.into(),
            trade_id: i,
            price: 1.0 + i as f64,
            quantity: 0.5,
            trade_time: now_ms,
            is_buyer_maker: i % 3 == 0,
            is_best_price_match: true,
        };
        let mut buf = Vec::with_capacity(event.encoded_len());
        event.encode(&mut buf).unwrap();
        js.publish(format!("exchange.{}", event.exchange), buf.into())
            .await
            .expect("publish")
            .await
            .expect("jetstream ack");
    }

    // ---- run the batcher ----------------------------------------------------------
    let sink = IcebergSink::connect(cfg.iceberg.clone())
        .await
        .expect("iceberg sink");
    let consumer_name = cfg.nats.consumer.clone();
    let receiver = source::spawn_nats_source(&cfg.nats, &cfg.batch)
        .await
        .expect("nats source");
    let (shutdown_tx, mut shutdown_rx) = watch::channel(());
    let batch_cfg = cfg.batch.clone();
    let run = tokio::spawn(async move {
        let mut sink = sink;
        batcher::run(receiver, &mut sink, batch_cfg, async move {
            let _ = shutdown_rx.changed().await;
        })
        .await
        .expect("batch loop")
    });

    // ---- poll the table until every published row is visible ----------------------
    let iceberg_cfg = cfg.iceberg.clone();
    let mut seen = 0usize;
    let deadline = std::time::Instant::now() + Duration::from_secs(180);
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_secs(3)).await;
        let sink = IcebergSink::connect(iceberg_cfg.clone())
            .await
            .expect("reconnect sink");
        let (count, ids) = read_marker_rows(sink.table(), &symbol).await;
        seen = count;
        if count as u64 >= publish_count {
            let unique: std::collections::HashSet<i64> = ids.iter().copied().collect();
            assert_eq!(
                unique.len(),
                ids.len(),
                "no duplicate trade_id for {symbol}"
            );
            break;
        }
    }

    let _ = shutdown_tx.send(());
    let stats = tokio::time::timeout(Duration::from_secs(60), run)
        .await
        .expect("batch loop stops")
        .expect("task join");

    assert!(
        seen as u64 >= publish_count,
        "only {seen}/{publish_count} rows reached the table (consumer {consumer_name}): {stats:?}"
    );
    assert_eq!(stats.failed_commits, 0, "no failed commits: {stats:?}");
    println!(
        "live e2e ok: published {publish_count} rows to {}, read {seen} back, stats {stats:?}",
        cfg.iceberg.table
    );
}

/// Rows of `symbol` currently visible in the table: count + trade ids.
async fn read_marker_rows(table: &iceberg::table::Table, symbol: &str) -> (usize, Vec<i64>) {
    let batches = table
        .scan()
        .select(["symbol", "trade_id"])
        .build()
        .unwrap()
        .to_arrow()
        .await
        .unwrap()
        .try_collect::<Vec<_>>()
        .await
        .unwrap();

    let mut ids = Vec::new();
    for batch in &batches {
        let symbols = batch
            .column_by_name("symbol")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let trade_ids = batch
            .column_by_name("trade_id")
            .unwrap()
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        for i in 0..symbols.len() {
            if symbols.value(i) == symbol {
                ids.push(trade_ids.value(i));
            }
        }
    }
    let n = ids.len();
    (n, ids)
}
