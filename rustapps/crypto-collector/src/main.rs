mod data;

use rdkafka::{ClientConfig, producer::FutureProducer};

use crate::data::{Exchange, binance_web_socket::BinanceExhange};

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_max_level(tracing::Level::INFO)
        .init();
    let redpanda_url = std::env::var("RP_URL").unwrap_or("127.0.0.1:9093".into());
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", redpanda_url)
        .set("debug", "broker,topic,msg")
        .set("message.timeout.ms", "5000")
        .create()
        .expect("Producer creation error");

    tracing::info!("Connection set up");
    let mut tasks_set = tokio::task::JoinSet::new();

    let binance = BinanceExhange {};
    tasks_set.spawn(async move { binance.the_big_loop(&producer).await });

    let results = tasks_set.join_all().await;

    for r in results {
        match r {
            Ok(_) => (),
            Err(e) => tracing::error!("Task failed: {}", e),
        }
    }

    tracing::info!("Done, exiting");

    Ok(())
}
