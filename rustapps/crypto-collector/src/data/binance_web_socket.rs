use futures::StreamExt;
use serde::{
    Deserialize, Deserializer, Serialize,
    de::{self},
};

use std::time::Duration;

const CONNECTION_TIMEOUT: Duration = Duration::from_secs(60 * 60 * 20);

use crate::data::Exchange;

use tokio_tungstenite::connect_async;

#[derive(Debug, Deserialize, Serialize)]
pub struct TradeEventBinance {
    #[serde(rename(deserialize = "e"))]
    pub event_type: String,

    #[serde(rename(deserialize = "E"))]
    pub event_time: u64,

    #[serde(rename(deserialize = "s"))]
    pub symbol: String,

    #[serde(rename(deserialize = "t"))]
    pub trade_id: u64,

    #[serde(rename(deserialize = "p"), deserialize_with = "string_to_f64")]
    pub price: f64,

    #[serde(rename(deserialize = "q"), deserialize_with = "string_to_f64")]
    pub quantity: f64,

    #[serde(rename(deserialize = "T"))]
    pub trade_time: u64,

    #[serde(rename(deserialize = "m"))]
    pub is_buyer_maker: bool,

    #[serde(rename(deserialize = "M"))]
    pub is_best_price_match: bool,
}

impl From<crate::data::binance_web_socket::TradeEventBinance> for crate::data::TradeEventProto {
    fn from(event: TradeEventBinance) -> Self {
        Self {
            event_time: event.event_time,
            symbol: event.symbol,
            exchange: "BINANCE".into(),
            trade_id: event.trade_id,
            price: event.price,
            quantity: event.quantity,
            trade_time: event.trade_time,
            is_buyer_maker: event.is_buyer_maker,
            is_best_price_match: event.is_best_price_match,
        }
    }
}

fn string_to_f64<'de, D>(deserilizer: D) -> Result<f64, D::Error>
where
    D: Deserializer<'de>,
{
    struct StrinOrFloat;
    impl<'de> de::Visitor<'de> for StrinOrFloat {
        type Value = f64;
        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a string containing a number")
        }
        fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            v.parse::<f64>().map_err(de::Error::custom)
        }
    }

    deserilizer.deserialize_any(StrinOrFloat)
}

pub struct BinanceExhange {}
impl Exchange<TradeEventBinance> for BinanceExhange {
    fn name(&self) -> &str {
        "BINANCE"
    }
    fn connection_manager(
        &self,
        sender: tokio::sync::mpsc::Sender<super::MyStream>,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send + 'static {
        async move {
            loop {
                let url = "wss://stream.binance.com:9443/ws/btcusdt@trade";
                tracing::info!("Setting up connection to ws");
                let (ws, _) = connect_async(url).await?;
                let (_, read) = ws.split();
                tracing::info!("Connection to ws set");
                let _ = sender.send(read).await;
                tracing::info!("Connection manager going to sleep");
                tokio::time::sleep(CONNECTION_TIMEOUT).await;
            }
        }
    }
}
