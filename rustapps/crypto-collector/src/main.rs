use anyhow::Context;
use crypto_data::data::{binance_web_socket::BinanceExhange, Exchange};
use crypto_data::publisher::Publisher;

struct AsyncNatsPublisher {
    js: async_nats::jetstream::Context,
}

impl Publisher for AsyncNatsPublisher {
    async fn publish_trade(
        &self,
        subject: String,
        event_bytes: Vec<u8>,
    ) -> anyhow::Result<()> {
        self.js.publish(subject, event_bytes.into()).await?;
        Ok(())
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_max_level(tracing::Level::INFO)
        .init();
    let nats_url = std::env::var("NATS_URL").unwrap_or("127.0.0.1:4222".into());
    let nc = async_nats::connect(&nats_url)
        .await
        .with_context(|| format!("failed to connect to NATS at {}", nats_url))?;

    let mut tasks_set = tokio::task::JoinSet::new();

    let js = async_nats::jetstream::new(nc);
    let publisher = AsyncNatsPublisher { js };

    let binance = BinanceExhange {};
    tasks_set.spawn(async move { binance.the_big_loop(&publisher).await });

    tasks_set.join_all().await;

    Ok(())
}
