use std::time::Duration;

use anyhow::Result;
use crypto_collector::build_publisher;
use crypto_data::data::{Exchange, binance_web_socket::BinanceExchange};
use tokio::task::JoinSet;

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_max_level(tracing::Level::INFO)
        .init();

    let publisher = build_publisher().await?;

    let mut tasks_set = JoinSet::new();

    let binance = BinanceExchange {};
    tasks_set.spawn(async move {
        let symbols = vec!["btcusdt", "ethusdt"];
        let mut backoff = Duration::from_secs(1);
        loop {
            let started = std::time::Instant::now();
            match binance.the_big_loop(&publisher, symbols.clone()).await {
                Ok(()) => tracing::info!("big loop ended; reconnecting"),
                Err(e) => tracing::error!("big loop failed: {e:?}; reconnecting"),
            }
            if started.elapsed() > Duration::from_secs(60) {
                backoff = Duration::from_secs(1);
            }
            tracing::info!("Reconnecting in {:?}", backoff);
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(60));
        }
    });

    tasks_set.join_all().await;

    Ok(())
}
