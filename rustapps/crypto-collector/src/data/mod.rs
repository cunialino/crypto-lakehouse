pub(crate) mod binance_web_socket;
use std::time::Duration;

use anyhow::Context;
use futures::stream::SplitStream;
use rdkafka::{
    producer::{FutureProducer, FutureRecord},
    util::Timeout,
};
use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Utf8Bytes};

use futures::StreamExt;

use prost::Message;

pub mod snazzy {
    pub mod items {
        include!(concat!(env!("OUT_DIR"), "/trade.data.rs"));
    }
}

use snazzy::items::TradeEventProto;

const TOPIC: &str = "exchange";

type MyStream = SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>;

pub(crate) trait Exchange<T: for<'de> serde::Deserialize<'de> + Into<TradeEventProto>>:
    Send + Sync
{
    fn name(&self) -> &str;
    fn handle_message(
        &self,
        message_txt: Utf8Bytes,
        producer: &FutureProducer,
    ) -> impl std::future::Future<Output = ()> + Send {
        let producer = producer.clone();

        async move {
            let my_data: T = serde_json::from_slice(message_txt.as_bytes()).unwrap();
            let proto: TradeEventProto = my_data.into();
            let mut buf = Vec::with_capacity(proto.encoded_len());

            proto.encode(&mut buf).unwrap();

            let record = FutureRecord::to(TOPIC).payload(&buf).key(self.name());
            match producer
                .send(record, Timeout::After(Duration::from_secs(5)))
                .await
            {
                Ok(delivery) => {
                    tracing::debug!(
                        "Delivered to partition {} at offset {}",
                        delivery.partition,
                        delivery.offset
                    );
                }
                Err((e, _)) => {
                    tracing::error!("Redpanda publish failed to {}: {:?}", TOPIC, e);
                }
            }
        }
    }
    fn connection_manager(
        &self,
        sender: tokio::sync::mpsc::Sender<MyStream>,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send + 'static;
    fn the_big_loop(
        &self,
        producer: &FutureProducer,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send {
        tracing::debug!("Starting {} big loop", self.name());
        async move {
            let (send, mut recv) = tokio::sync::mpsc::channel(1);
            let fut = self.connection_manager(send);
            tokio::spawn(async move {
                match fut.await {
                    Ok(()) => {}
                    Err(e) => tracing::error!("Connection manager failed: {e:?}"),
                }
            });
            let mut read = recv.recv().await.context("Could not init reader")?;
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
                                    Ok(tokio_tungstenite::tungstenite::Message::Text(txt)) => {
                                        self.handle_message(txt, producer).await;
                                    },
                                    Ok(_) => {},
                                    Err(e) => {
                                        tracing::error!("websocket error: {}", e);
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
