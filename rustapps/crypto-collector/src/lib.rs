use anyhow::Context;
use crypto_data::publisher::{LoggingPublisher, Publisher};
use tracing::info;

pub struct AsyncNatsPublisher {
    js: async_nats::jetstream::Context,
}

impl Publisher for AsyncNatsPublisher {
    async fn publish_trade(&self, subject: String, event_bytes: Vec<u8>) -> anyhow::Result<()> {
        self.js.publish(subject, event_bytes.into()).await?;
        Ok(())
    }
}

pub enum Either {
    Nats(Box<AsyncNatsPublisher>),
    Logging(LoggingPublisher),
}

impl Publisher for Either {
    async fn publish_trade(&self, subject: String, event_bytes: Vec<u8>) -> anyhow::Result<()> {
        match self {
            Either::Nats(p) => p.publish_trade(subject, event_bytes).await,
            Either::Logging(p) => p.publish_trade(subject, event_bytes).await,
        }
    }
}

pub async fn build_publisher() -> anyhow::Result<Either> {
    match std::env::var("NATS_URL") {
        Ok(url) if !url.is_empty() => {
            let nc = async_nats::connect(&url)
                .await
                .with_context(|| format!("failed to connect to NATS at {}", url))?;
            let js = async_nats::jetstream::new(nc);
            Ok(Either::Nats(Box::new(AsyncNatsPublisher { js })))
        }
        _ => {
            info!("NATS_URL not set; using LoggingPublisher (no NATS required)");
            Ok(Either::Logging(LoggingPublisher))
        }
    }
}
