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

impl From<TradeEventBinance> for crate::data::TradeEventProto {
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

#[derive(Debug, Deserialize)]
pub struct BinanceStreamPayload {
    #[allow(dead_code)]
    pub stream: String,
    pub data: TradeEventBinance,
}

impl From<BinanceStreamPayload> for crate::data::TradeEventProto {
    fn from(payload: BinanceStreamPayload) -> Self {
        payload.data.into()
    }
}

fn string_to_f64<'de, D>(deserilizer: D) -> Result<f64, D::Error>
where
    D: Deserializer<'de>,
{
    struct StringOrFloat;
    impl<'de> de::Visitor<'de> for StringOrFloat {
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

    deserilizer.deserialize_any(StringOrFloat)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Example payload from the Binance websocket docs
    // (https://github.com/binance/binance-spot-api-docs/blob/master/web-socket-streams.md)
    const SAMPLE_TRADE: &str = r#"{
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

    #[test]
    fn parses_binance_trade_json() {
        let trade: TradeEventBinance = serde_json::from_str(SAMPLE_TRADE).unwrap();
        assert_eq!(trade.event_type, "trade");
        assert_eq!(trade.event_time, 1672515782136);
        assert_eq!(trade.symbol, "BNBBTC");
        assert_eq!(trade.trade_id, 12345);
        assert_eq!(trade.price, 0.00000050_f64);
        assert_eq!(trade.quantity, 1000.0);
        assert_eq!(trade.trade_time, 1672515782137);
        assert!(!trade.is_buyer_maker);
        assert!(trade.is_best_price_match);
    }

    #[test]
    fn converts_to_proto_and_tags_exchange() {
        let trade: TradeEventBinance = serde_json::from_str(SAMPLE_TRADE).unwrap();
        let proto: crate::data::TradeEventProto = trade.into();
        assert_eq!(proto.exchange, "BINANCE");
        assert_eq!(proto.symbol, "BNBBTC");
        assert_eq!(proto.event_time, 1672515782136);
        assert_eq!(proto.trade_id, 12345);
        assert_eq!(proto.price, 0.00000050_f64);
        assert_eq!(proto.quantity, 1000.0);
        assert_eq!(proto.is_buyer_maker, false);
        assert_eq!(proto.is_best_price_match, true);
    }

    #[test]
    fn parses_stream_payload_wrapper() {
        let json = format!(r#"{{"stream":"bnbbtc@trade","data":{}}}"#, SAMPLE_TRADE);
        let payload: BinanceStreamPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(payload.stream, "bnbbtc@trade");
        let proto: crate::data::TradeEventProto = payload.into();
        assert_eq!(proto.symbol, "BNBBTC");
        assert_eq!(proto.price, 0.00000050_f64);
    }

    #[test]
    fn numeric_fields_accept_plain_float() {
        let json = SAMPLE_TRADE
            .replace("\"0.00000050\"", "0.00000050")
            .replace("\"1000.00000000\"", "1000.0");
        let trade: TradeEventBinance = serde_json::from_str(&json).unwrap();
        assert_eq!(trade.price, 0.00000050_f64);
        assert_eq!(trade.quantity, 1000.0);
    }

    #[test]
    fn builds_subject_connection_string() {
        let exchange = BinanceExchange {};
        let streams: Vec<String> = vec!["BTcUsdt", "ETHUSDT"]
            .into_iter()
            .map(|s| format!("{}@trade", s.to_lowercase()))
            .collect();
        assert_eq!(streams, vec!["btcusdt@trade", "ethusdt@trade"]);
        let name = <BinanceExchange as Exchange<BinanceStreamPayload, crate::publisher::LoggingPublisher>>::name(&exchange);
        assert_eq!(name, "BINANCE");
    }
}

pub struct BinanceExchange {}
impl<P: crate::publisher::Publisher> Exchange<BinanceStreamPayload, P> for BinanceExchange {
    fn name(&self) -> &str {
        "BINANCE"
    }
    fn connection_manager<I, S>(
        &self,
        sender: tokio::sync::mpsc::Sender<super::MyStream>,
        symbols: I,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send + 'static
    where
        I: IntoIterator<Item = S> + std::marker::Send,
        S: AsRef<str>,
    {
        let streams: Vec<String> = symbols
            .into_iter()
            .map(|s| format!("{}@trade", s.as_ref().to_lowercase()))
            .collect();
        async move {
            loop {
                let url = format!(
                    "wss://stream.binance.com:9443/stream?streams={}",
                    streams.join("/")
                );

                tracing::info!("Setting up connection to ws");
                let (ws, _): (_, _) = connect_async(url).await?;
                let (_, read) = ws.split();
                tracing::info!("Connection to ws set");
                sender.send(read).await?;
                tracing::info!("Connection manager going to sleep");
                tokio::time::sleep(CONNECTION_TIMEOUT).await;
            }
        }
    }
}