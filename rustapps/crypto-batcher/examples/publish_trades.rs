//! Local E2E helper: publishes synthetic trade events to the JetStream stream the
//! batcher consumes.
//!
//! ```sh
//! NATS_URL=nats://127.0.0.1:14222 cargo run -p crypto-batcher --example publish_trades -- 2000
//! ```
//!
//! Creates the stream (`tradesstream`, subjects `exchange.*`) if it does not exist, which
//! mirrors what the jetstream-controller does in-cluster
//! (`deploy/base/collector/stream.yaml`).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_nats::jetstream;
use async_nats::jetstream::stream::Config as StreamConfig;
use crypto_batcher::schema::TradeEvent;
use prost::Message as _;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://127.0.0.1:14222".into());
    let stream_name = std::env::var("TRADES_STREAM").unwrap_or_else(|_| "tradesstream".to_string());
    let count: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(1_000);

    let js = jetstream::new(async_nats::connect(&nats_url).await?);

    let mut stream = js
        .get_or_create_stream(StreamConfig {
            name: stream_name.clone(),
            subjects: vec!["exchange.*".into()],
            max_age: Duration::from_secs(72 * 3600),
            ..Default::default()
        })
        .await?;

    let now_ms = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64;
    // Spread events over two days so `day(trade_ts)` produces two partitions.
    let day_ms = 24 * 60 * 60 * 1000;

    let mut published = 0u64;
    for i in 0..count {
        let day = (i / (count / 2).max(1)) % 2;
        let exchange = if i % 3 == 0 { "OKX" } else { "BINANCE" };
        let event = TradeEvent {
            event_time: now_ms,
            symbol: if exchange == "OKX" {
                "ETHUSDT"
            } else {
                "BTCUSDT"
            }
            .into(),
            exchange: exchange.into(),
            trade_id: i,
            price: 50_000.0 + (i % 100) as f64,
            quantity: 0.001 * (i % 50) as f64,
            trade_time: now_ms - day * day_ms,
            is_buyer_maker: i % 2 == 0,
            is_best_price_match: true,
        };

        let mut buf = Vec::with_capacity(event.encoded_len());
        event.encode(&mut buf)?;
        let subject = format!("exchange.{}", event.exchange);
        js.publish(subject, buf.clone().into()).await?.await?;
        published += 1;
    }

    let info = stream.info().await?;
    println!(
        "published {published} trade events to stream '{}' (state: {} messages)",
        stream_name, info.state.messages
    );
    Ok(())
}
