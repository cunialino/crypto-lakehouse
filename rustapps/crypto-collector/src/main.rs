use anyhow::Context;
use crypto_data::data::{binance_web_socket::BinanceExhange, Exchange};
use crypto_data::publisher::{LoggingPublisher, Publisher};
use tracing::info;

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

enum Either {
    Nats(AsyncNatsPublisher),
    Logging(LoggingPublisher),
}

impl Publisher for Either {
    async fn publish_trade(
        &self,
        subject: String,
        event_bytes: Vec<u8>,
    ) -> anyhow::Result<()> {
        match self {
            Either::Nats(p) => p.publish_trade(subject, event_bytes).await,
            Either::Logging(p) => p.publish_trade(subject, event_bytes).await,
        }
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_max_level(tracing::Level::INFO)
        .init();

    let publisher: Either = match std::env::var("NATS_URL") {
        Ok(url) if !url.is_empty() => {
            let nc = async_nats::connect(&url)
                .await
                .with_context(|| format!("failed to connect to NATS at {}", url))?;
            let js = async_nats::jetstream::new(nc);
            Either::Nats(AsyncNatsPublisher { js })
        }
        _ => {
            info!("NATS_URL not set; using LoggingPublisher (no NATS required)");
            Either::Logging(LoggingPublisher)
        }
    };

    let mut tasks_set = tokio::task::JoinSet::new();

    let binance = BinanceExhange {};
    tasks_set.spawn(async move { binance.the_big_loop(&publisher, vec!["btcusdt", "ethusdt"]).await });

    tasks_set.join_all().await;

    Ok(())
}
