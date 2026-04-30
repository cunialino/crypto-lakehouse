pub trait Publisher: Send + Sync {
    fn publish_trade(
        &self,
        subject: String,
        event_bytes: Vec<u8>,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;
}
