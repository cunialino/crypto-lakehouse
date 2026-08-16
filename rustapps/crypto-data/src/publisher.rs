pub trait Publisher: Send + Sync {
    fn publish_trade(
        &self,
        subject: String,
        event_bytes: Vec<u8>,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;
}

pub struct LoggingPublisher;

impl Publisher for LoggingPublisher {
    async fn publish_trade(&self, subject: String, event_bytes: Vec<u8>) -> anyhow::Result<()> {
        tracing::info!(subject, len = event_bytes.len(), "publish_trade");
        Ok(())
    }
}
