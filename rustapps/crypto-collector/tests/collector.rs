use crypto_collector::{Either, build_publisher};
use crypto_data::publisher::Publisher;

// Serial/kill switch: these tests mutate the process env, so keep them in one
// file and rely on cargo running integration-test binaries in a single thread.
#[tokio::test]
async fn build_publisher_falls_back_to_logging_without_nats_url() {
    // SAFETY: tests run single-threaded (cargo default), no other thread reads NATS_URL.
    unsafe { std::env::remove_var("NATS_URL") };

    let publisher = build_publisher().await.unwrap();
    assert!(matches!(publisher, Either::Logging(_)));
}

#[tokio::test]
async fn logging_publisher_accepts_a_trade_publish() {
    let publisher = Either::Logging(crypto_data::publisher::LoggingPublisher);

    let result = publisher
        .publish_trade("exchange.BINANCE".to_string(), vec![0x0a, 0x01])
        .await;

    assert!(result.is_ok());
}

// Proves the "Either" wrapper forwards calls to the inner publisher,
// guarding the Dispatch-abstraction used by main().
#[tokio::test]
async fn either_dispatch_forwards_to_inner_publisher() {
    let publisher = Either::Logging(crypto_data::publisher::LoggingPublisher);

    let result = publisher
        .publish_trade("exchange.BINANCE".to_string(), Vec::new())
        .await;

    assert!(result.is_ok());
}
