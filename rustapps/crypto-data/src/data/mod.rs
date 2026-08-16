pub mod binance_web_socket;
use anyhow::Context;
use futures::stream::SplitStream;
use prost::Message;
use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Utf8Bytes};

use futures::StreamExt;
use tokio_tungstenite::tungstenite::Message as TMsg;

pub mod snazzy {
    pub mod items {
        include!(concat!(env!("OUT_DIR"), "/trade.data.rs"));
    }
}

pub use snazzy::items::TradeEventProto;

type MyStream = SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>;

pub trait Exchange<
    T: for<'de> serde::Deserialize<'de> + Into<TradeEventProto>,
    P: crate::publisher::Publisher,
>: Send + Sync
{
    fn name(&self) -> &str;
    fn handle_message(
        &self,
        message_txt: Utf8Bytes,
        publisher: &P,
    ) -> impl std::future::Future<Output = ()> + Send {
        async move {
            let my_data: T = serde_json::from_slice(message_txt.as_bytes()).unwrap();
            let subject = format!("exchange.{}", self.name());
            let proto: TradeEventProto = my_data.into();
            let mut buf = Vec::with_capacity(proto.encoded_len());

            proto
                .encode(&mut buf)
                .map_err(|e| {
                    eprintln!("Failed to encode: {}", e);
                })
                .unwrap();
            match publisher.publish_trade(subject, buf).await {
                Ok(_) => {}
                Err(e) => {
                    tracing::error!("NATS publish failed: {:?}", e);
                }
            }
        }
    }
    fn connection_manager<I, S>(
        &self,
        sender: tokio::sync::mpsc::Sender<MyStream>,
        symbols: I,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send + 'static
    where
        I: IntoIterator<Item = S> + std::marker::Send,
        S: AsRef<str>;
    fn the_big_loop<I, S>(
        &self,
        publisher: &P,
        symbols: I,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send
    where
        I: IntoIterator<Item = S> + std::marker::Send,
        S: AsRef<str>,
    {
        tracing::debug!("Starting {} big loop", self.name());
        async move {
            let (send, mut recv) = tokio::sync::mpsc::channel(1);
            let fut = self.connection_manager(send, symbols);
            tokio::spawn(async move {
                match fut.await {
                    Ok(()) => {}
                    Err(e) => tracing::error!("Connection manager failed: {e:?}"),
                }
            });
            let mut read: MyStream = recv.recv().await.context("Could not init reader")?;
            loop {
                tokio::select! {
                    new_read = recv.recv() => {
                        match new_read {
                            Some(r) => { read = r }
                            None => {
                                tracing::error!("Manager died");
                                break;
                            }
                        }
                    }
                    msg_res = read.next() => {
                        match msg_res {
                            Some(msg) => {
                                match msg {
                                    Ok(TMsg::Text(txt)) => {
                                        self.handle_message(txt, publisher).await;
                                    },
                                    Ok(b) => {tracing::warn!("Unknown behaviour: {}", b)},
                                    Err(e) => {
                                        eprintln!("websocket error: {}", e);
                                        break;
                                    }
                                }
                            }
                            None => {
                                tracing::error!("No next message");
                                break;
                            }
                        }
                    }
                }
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::binance_web_socket::{BinanceExchange, TradeEventBinance};
    use crate::publisher::Publisher;
    use std::sync::{Arc, Mutex};

    #[derive(Default, Clone)]
    struct CapturingPublisher {
        published: Arc<Mutex<Vec<(String, Vec<u8>)>>>,
    }

    impl CapturingPublisher {
        fn recorded(&self) -> Vec<(String, Vec<u8>)> {
            self.published.lock().unwrap().clone()
        }
    }

    impl Publisher for CapturingPublisher {
        async fn publish_trade(&self, subject: String, event_bytes: Vec<u8>) -> anyhow::Result<()> {
            self.published.lock().unwrap().push((subject, event_bytes));
            Ok(())
        }
    }

    const SAMPLE_TRADE: &str = r#"{
        "stream": "bnbbtc@trade",
        "data": {
            "e": "trade",
            "E": 1672515782136,
            "s": "BNBBTC",
            "t": 12345,
            "p": "0.00000050",
            "q": "1000.00000000",
            "T": 1672515782137,
            "m": false,
            "M": true
        }
    }"#;

    #[tokio::test]
    async fn handle_message_parses_encodes_and_publishes() {
        let publisher = CapturingPublisher::default();
        let exchange = BinanceExchange {};

        exchange
            .handle_message(Utf8Bytes::from(SAMPLE_TRADE), &publisher)
            .await;

        let recorded = publisher.recorded();
        assert_eq!(recorded.len(), 1, "exactly one trade should be published");

        let (subject, bytes) = &recorded[0];
        assert_eq!(subject, "exchange.BINANCE");

        let proto = TradeEventProto::decode(bytes.as_slice()).unwrap();
        assert_eq!(proto.exchange, "BINANCE");
        assert_eq!(proto.symbol, "BNBBTC");
        assert_eq!(proto.trade_id, 12345);
        assert_eq!(proto.price, 0.00000050_f64);
        assert_eq!(proto.quantity, 1000.0);
        assert_eq!(proto.is_buyer_maker, false);
    }

    #[tokio::test]
    async fn handle_message_propagates_publisher_errors() {
        let exchange = BinanceExchange {};

        #[derive(Clone)]
        struct FailingPublisher;

        impl Publisher for FailingPublisher {
            async fn publish_trade(&self, _: String, _: Vec<u8>) -> anyhow::Result<()> {
                Err(anyhow::anyhow!("nats down"))
            }
        }

        let publisher = FailingPublisher;
        exchange
            .handle_message(Utf8Bytes::from(SAMPLE_TRADE), &publisher)
            .await;
    }

    #[test]
    fn proto_roundtrip_is_binary_compatible() {
        let raw = r#"{
            "e": "trade",
            "E": 1672515782136,
            "s": "BNBBTC",
            "t": 12345,
            "p": "0.00000050",
            "q": "1000.00000000",
            "T": 1672515782137,
            "m": false,
            "M": true
        }"#;
        let trade: TradeEventBinance = serde_json::from_str(raw).unwrap();
        let proto: TradeEventProto = trade.into();
        let mut buf = Vec::with_capacity(proto.encoded_len());
        proto.encode(&mut buf).unwrap();
        let decoded = TradeEventProto::decode(buf.as_slice()).unwrap();
        assert_eq!(decoded, proto);
    }
}