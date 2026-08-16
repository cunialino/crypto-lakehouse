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
        binance
            .the_big_loop(&publisher, vec!["btcusdt", "ethusdt"])
            .await
    });

    tasks_set.join_all().await;

    Ok(())
}
