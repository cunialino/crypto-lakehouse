pub(crate) mod binance_web_socket;
use anyhow::Context;
use futures::stream::SplitStream;
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

type MyStream = SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>;

pub(crate) trait Exchange<T: for<'de> serde::Deserialize<'de> + Into<TradeEventProto>>:
    Send + Sync
{
    fn name(&self) -> &str;
    fn handle_message(
        &self,
        message_txt: Utf8Bytes,
        js: &async_nats::jetstream::Context,
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
            match js.publish(subject.to_string(), buf.into()).await {
                Ok(ack) => match ack.await {
                    Ok(_) => {}
                    Err(e) => {
                        tracing::error!("NATS ack failed: {:?}", e);
                    }
                },
                Err(e) => {
                    eprintln!("failed to publish to {}: {:?}", subject, e);
                }
            }
            ()
        }
    }
    fn connection_manager(
        &self,
        sender: tokio::sync::mpsc::Sender<MyStream>,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send + 'static;
    fn the_big_loop(
        &self,
        js: &async_nats::jetstream::Context,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send {
        tracing::debug!("Starting {} big loop", self.name());
        async move {
            let (send, mut recv) = tokio::sync::mpsc::channel(1);
            let _ = tokio::spawn(self.connection_manager(send));
            let mut read = recv.recv().await.context("Could init reader")?;
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
                                        self.handle_message(txt, js).await;
                                    },
                                    Ok(_) => {},
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
