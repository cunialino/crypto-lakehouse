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

use async_nats::jetstream::consumer::{AckPolicy, DeliverPolicy};
use async_nats::jetstream::stream::Stream;
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

    // Creating the consumer is the jetstream-controller's job in-cluster, so the test does
    // it here instead: the batcher only ever reads and validates what it points at.
    ensure_test_consumer(&stream, &cfg).await;

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
    let (receiver, batch_cfg) = source::spawn_nats_source(&cfg.nats, cfg.batch.clone())
        .await
        .expect("nats source");
    let (shutdown_tx, mut shutdown_rx) = watch::channel(());
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

/// Creates the consumer the batcher points at, the way `consumer.yaml` does in-cluster.
/// The batcher never writes JetStream state, so a live-stack run has to supply the cursor.
///
/// Recreated rather than reused: a durable consumer keeps whatever config it was created
/// with and nothing here repairs it, so the run starts from exactly the shape below.
async fn ensure_test_consumer(stream: &Stream, cfg: &Config) {
    assert!(
        cfg.nats.consumer.ends_with("-e2e") || cfg.nats.consumer.starts_with("it-"),
        "live_stack needs a throwaway BATCHER_CONSUMER, not '{}'",
        cfg.nats.consumer
    );
    stream.delete_consumer(&cfg.nats.consumer).await.ok();
    stream
        .get_or_create_consumer(
            &cfg.nats.consumer,
            async_nats::jetstream::consumer::pull::Config {
                name: Some(cfg.nats.consumer.clone()),
                durable_name: Some(cfg.nats.consumer.clone()),
                filter_subject: cfg.nats.subject_filter.clone(),
                deliver_policy: DeliverPolicy::All,
                ack_policy: AckPolicy::Explicit,
                ack_wait: Duration::from_secs(300),
                max_deliver: -1,
                max_ack_pending: 20_000,
                ..Default::default()
            },
        )
        .await
        .expect("consumer");
}

type Breaker = Box<dyn Fn(&mut async_nats::jetstream::consumer::pull::Config)>;

fn as_breaker(f: impl Fn(&mut async_nats::jetstream::consumer::pull::Config) + 'static) -> Breaker {
    Box::new(f)
}

/// What reaches the batch loop is the consumer-capped policy, not the ConfigMap's — the
/// pure `fit_batch_policy` test cannot show that the cap survives the real load path.
#[tokio::test]
#[ignore = "requires teststack/: NATS (see deploy/docs/rust-batcher-guide.md)"]
async fn live_stack_caps_the_micro_batch_to_the_consumer() {
    let mut cfg = Config::from_env().expect("config from env");
    let js = async_nats::jetstream::new(
        async_nats::connect(&cfg.nats.url)
            .await
            .expect("connect nats"),
    );
    let stream = js
        .get_or_create_stream(async_nats::jetstream::stream::Config {
            name: cfg.nats.stream.clone(),
            subjects: vec!["exchange.*".into()],
            ..Default::default()
        })
        .await
        .expect("stream");
    ensure_test_consumer(&stream, &cfg).await; // ack_wait 300 s, max_ack_pending 20 000

    cfg.batch.flush_interval = Duration::from_secs(900);
    cfg.batch.max_records = 50_000;
    cfg.nats.fetch_batch = 20_000;

    let (_receiver, policy) = source::spawn_nats_source(&cfg.nats, cfg.batch.clone())
        .await
        .expect("nats source");
    assert_eq!(
        policy.flush_interval,
        Duration::from_secs(72),
        "(300 s - 10 s) / 4"
    );
    assert_eq!(policy.max_records, 10_000, "max_ack_pending / 2");
}

/// The startup guard against a real server: a consumer whose config cannot carry the
/// batching policy has to stop the process, not quietly duplicate or drop rows.
#[tokio::test]
#[ignore = "requires teststack/: NATS (see deploy/docs/rust-batcher-guide.md)"]
async fn live_stack_refuses_a_consumer_that_contradicts_the_batching_policy() {
    let mut cfg = Config::from_env().expect("config from env");
    let js = async_nats::jetstream::new(
        async_nats::connect(&cfg.nats.url)
            .await
            .expect("connect nats"),
    );
    let stream = js
        .get_or_create_stream(async_nats::jetstream::stream::Config {
            name: cfg.nats.stream.clone(),
            subjects: vec!["exchange.*".into()],
            ..Default::default()
        })
        .await
        .expect("stream");

    for (name, break_config) in [
        // Would never see OKX trades.
        (
            "it-narrow-filter",
            as_breaker(|c| c.filter_subject = "exchange.BINANCE".into()),
        ),
        // Would stall: one micro-batch alone exceeds the unacked cap.
        ("it-starved", as_breaker(|c| c.max_ack_pending = 4)),
    ] {
        let mut config = async_nats::jetstream::consumer::pull::Config {
            name: Some(name.to_string()),
            durable_name: Some(name.to_string()),
            filter_subject: cfg.nats.subject_filter.clone(),
            ack_policy: AckPolicy::Explicit,
            // Comfortably inside the policy, so only the break under test is reported.
            ack_wait: Duration::from_secs(300),
            max_ack_pending: 20_000,
            ..Default::default()
        };
        break_config(&mut config);
        // A durable consumer keeps the config it was created with, so start from a clean one.
        stream.delete_consumer(name).await.ok();
        stream
            .get_or_create_consumer(name, config)
            .await
            .expect("broken consumer");

        cfg.nats.consumer = name.to_string();
        let Err(err) = source::spawn_nats_source(&cfg.nats, cfg.batch.clone()).await else {
            panic!("consumer '{name}' contradicts the policy but the source started anyway");
        };
        println!("{name}: {err}");
    }
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
