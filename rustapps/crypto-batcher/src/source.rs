//! Trade event sources.
//!
//! The batcher proper is source-agnostic: it consumes an [`mpsc`] stream of
//! [`Envelope`]s, each carrying an [`Ack`] handle. Production wires that channel to a
//! NATS JetStream pull consumer; tests wire it to an in-memory feeder. Keeping the
//! ack handle next to the row is what makes "ack only after the Iceberg commit"
//! expressible without leaking JetStream types into the batching core.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use async_nats::jetstream;
use async_nats::jetstream::consumer::{self, AckPolicy, DeliverPolicy, PullConsumer};
use async_nats::jetstream::stream::Stream;
use futures::StreamExt;
use prost::Message as _;
use tokio::sync::mpsc;

use crate::schema::TradeEvent;

/// Proof that a row can be removed from the source.
///
/// Acking is only done after the micro-batch has been committed to Iceberg, so a crash
/// anywhere before that leaves the messages unacked and JetStream redelivers them
/// (at-least-once, same contract Arroyo had, minus its checkpoint replay).
pub enum Ack {
    // Boxed: `jetstream::Message` is ~400 bytes and most acks are this variant.
    Nats(Box<jetstream::Message>),
    /// Test/loopback hook: counts successful acks.
    Counter(Arc<AtomicU64>),
    /// Synthetic row (backfill, test) with nothing to acknowledge.
    None,
}

impl Ack {
    pub async fn ack(&self) -> Result<()> {
        match self {
            Ack::Nats(msg) => msg
                .ack()
                .await
                .map_err(|e| anyhow::anyhow!("nats ack failed: {e}"))?,
            Ack::Counter(c) => {
                c.fetch_add(1, Ordering::Relaxed);
            }
            Ack::None => {}
        }
        Ok(())
    }
}

/// One decoded trade plus the handle that acknowledges it upstream.
pub struct Envelope {
    pub event: TradeEvent,
    /// Encoded payload size; drives the byte-based flush trigger.
    pub bytes: usize,
    pub ack: Ack,
}

/// Decodes one JetStream payload. Bad payloads are reported as `None` so the caller can
/// drop-and-ack them instead of poisoning the queue (Arroyo used `bad_data: drop`).
pub fn decode(payload: &[u8]) -> Option<TradeEvent> {
    TradeEvent::decode(payload).ok()
}

/// A feed of trade envelopes handed to the micro-batcher.
pub struct TradeReceiver {
    rx: mpsc::Receiver<Envelope>,
}

impl TradeReceiver {
    pub async fn recv(&mut self) -> Option<Envelope> {
        self.rx.recv().await
    }
}

/// Raw channel behind [`TradeReceiver`]; used by tests (and any future second source)
/// to feed the batcher without a NATS server.
pub fn trade_channel(capacity: usize) -> (mpsc::Sender<Envelope>, TradeReceiver) {
    let (tx, rx) = mpsc::channel(capacity.max(1));
    (tx, TradeReceiver { rx })
}

/// Spawns the JetStream pull loop and returns the receiving end.
///
/// The consumer is durable and explicit-ack: its ack floor *is* the pipeline offset, so
/// there is no separate state store to keep in sync. `max_ack_pending` is sized from the
/// batching policy because in-flight-but-unacked rows count against it.
pub async fn spawn_nats_source(
    cfg: &crate::config::NatsConfig,
    batch: &crate::config::BatchConfig,
) -> Result<TradeReceiver> {
    let client = async_nats::connect(&cfg.url)
        .await
        .with_context(|| format!("failed to connect to NATS at {}", cfg.url))?;
    let js = jetstream::new(client);

    let stream: Stream = js
        .get_stream(&cfg.stream)
        .await
        .with_context(|| format!("JetStream stream '{}' not found", cfg.stream))?;

    let consumer = ensure_consumer(&stream, cfg, batch).await?;
    tracing::info!(
        stream = %cfg.stream,
        consumer = %cfg.consumer,
        filter = %cfg.subject_filter,
        "nats source started"
    );

    // Backpressure: at most one full micro-batch in flight towards the batcher.
    let (tx, rx) = mpsc::channel(batch.max_records.max(1));
    let fetch_batch = cfg.fetch_batch.max(1);
    let fetch_expires = cfg.fetch_expires;

    tokio::spawn(async move {
        loop {
            match consumer
                .fetch()
                .max_messages(fetch_batch)
                .expires(fetch_expires)
                .messages()
                .await
            {
                Ok(mut batch_stream) => {
                    let mut got = 0usize;
                    while let Some(msg) = batch_stream.next().await {
                        match msg {
                            Ok(msg) => {
                                got += 1;
                                let bytes = msg.payload.len();
                                let envelope = match decode(&msg.payload) {
                                    Some(event) => Envelope {
                                        event,
                                        bytes,
                                        ack: Ack::Nats(Box::new(msg)),
                                    },
                                    None => {
                                        tracing::warn!(
                                            bytes,
                                            "undecodable payload; dropping (bad_data=drop)"
                                        );
                                        if let Err(e) = msg.ack().await {
                                            tracing::error!(
                                                "failed to ack undecodable payload: {e}"
                                            );
                                        }
                                        continue;
                                    }
                                };
                                if tx.send(envelope).await.is_err() {
                                    tracing::info!("batcher gone; nats source stopping");
                                    return;
                                }
                            }
                            Err(e) => {
                                tracing::error!("fetch stream error: {e}");
                                break;
                            }
                        }
                    }
                    if got == 0 {
                        // NO_MESSAGES / expired pull: back off a hair instead of hammering the API.
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }
                Err(e) => {
                    tracing::error!("pull fetch failed: {e}; retrying in 1s");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    });

    Ok(TradeReceiver { rx })
}

/// The `ack_wait` actually used for the consumer.
///
/// A buffered-but-unacked row is redelivered once `ack_wait` elapses, so an `ack_wait`
/// close to `flush_interval` would hand the batcher duplicates of rows it is *still
/// holding*. Clamp it well above the flush interval instead of relying on the operator
/// to keep the two knobs consistent.
pub fn effective_ack_wait(configured: Duration, flush_interval: Duration) -> Duration {
    let floor = flush_interval * 4 + Duration::from_secs(10);
    if configured < floor {
        tracing::warn!(
            configured_secs = configured.as_secs(),
            used_secs = floor.as_secs(),
            flush_interval_secs = flush_interval.as_secs(),
            "BATCHER_ACK_WAIT_SECS too close to BATCH_FLUSH_INTERVAL_SECS; raising ack_wait"
        );
        floor
    } else {
        configured
    }
}

async fn ensure_consumer(
    stream: &Stream,
    cfg: &crate::config::NatsConfig,
    batch: &crate::config::BatchConfig,
) -> Result<PullConsumer> {
    // Enough headroom for one micro-batch in the batcher plus one in flight to it, so the
    // pull never stalls because unacked rows hit the cap.
    let max_ack_pending = (batch.max_records * 2).max(cfg.fetch_batch * 2) as i64;
    let ack_wait = effective_ack_wait(cfg.ack_wait, batch.flush_interval);

    let config = consumer::pull::Config {
        name: Some(cfg.consumer.clone()),
        durable_name: Some(cfg.consumer.clone()),
        filter_subject: cfg.subject_filter.clone(),
        deliver_policy: DeliverPolicy::All,
        ack_policy: AckPolicy::Explicit,
        ack_wait,
        max_deliver: -1, // unlimited: redelivery is the only recovery path we have
        max_ack_pending,
        ..Default::default()
    };

    let mut consumer = stream
        .get_or_create_consumer(&cfg.consumer, config.clone())
        .await
        .with_context(|| format!("failed to get/create consumer '{}'", cfg.consumer))?;

    // A durable consumer keeps whatever config it was first created with, so a changed
    // `ack_wait` / `max_ack_pending` / filter in the environment would silently NOT apply
    // (which matters: a stale small `max_ack_pending` stalls the pull, a stale small
    // `ack_wait` duplicates whole batches). Reconcile the drift instead.
    let mut info = stream.consumer_info(&cfg.consumer).await.ok();
    if let Some(current) = info.as_ref()
        && (current.config.ack_wait != config.ack_wait
            || current.config.max_ack_pending != config.max_ack_pending
            || current.config.filter_subject != config.filter_subject)
    {
        tracing::info!(
            consumer = %cfg.consumer,
            old_ack_wait_secs = current.config.ack_wait.as_secs(),
            new_ack_wait_secs = config.ack_wait.as_secs(),
            old_max_ack_pending = current.config.max_ack_pending,
            new_max_ack_pending = config.max_ack_pending,
            "updating consumer config"
        );
        stream
            .update_consumer(config.clone())
            .await
            .with_context(|| format!("failed to update consumer '{}'", cfg.consumer))?;
        consumer = stream
            .get_consumer(&cfg.consumer)
            .await
            .map_err(|e| anyhow::anyhow!("failed to reload consumer: {e}"))
            .with_context(|| format!("failed to reload consumer '{}'", cfg.consumer))?;
        // Re-read so the log below reports what the server actually has.
        info = stream.consumer_info(&cfg.consumer).await.ok();
    }

    if let Some(info) = info {
        tracing::info!(
            consumer = %cfg.consumer,
            ack_wait_secs = info.config.ack_wait.as_secs(),
            max_ack_pending = info.config.max_ack_pending,
            num_pending = info.num_pending,
            num_ack_pending = info.num_ack_pending,
            num_redelivered = info.num_redelivered,
            "consumer ready"
        );
    }

    Ok(consumer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_rejects_garbage_and_accepts_proto() {
        assert!(decode(b"not a proto at all \x00\x00".as_slice()).is_none());

        let event = TradeEvent {
            event_time: 1,
            symbol: "ETHUSDT".into(),
            exchange: "BINANCE".into(),
            trade_id: 7,
            price: 10.0,
            quantity: 1.0,
            trade_time: 2,
            is_buyer_maker: false,
            is_best_price_match: true,
        };
        let mut buf = Vec::new();
        event.encode(&mut buf).unwrap();

        let decoded = decode(&buf).unwrap();
        assert_eq!(decoded, event);
    }

    #[test]
    fn ack_wait_is_clamped_above_the_flush_interval() {
        use std::time::Duration;
        // 5 s flush interval => floor of 4*5+10 = 30 s.
        assert_eq!(
            effective_ack_wait(Duration::from_secs(15), Duration::from_secs(5)),
            Duration::from_secs(30)
        );
        // A sane operator setting is left alone.
        assert_eq!(
            effective_ack_wait(Duration::from_secs(300), Duration::from_secs(30)),
            Duration::from_secs(300)
        );
    }

    #[tokio::test]
    async fn ack_counter_counts() {
        let counter = Arc::new(AtomicU64::new(0));
        let envelope = Envelope {
            event: TradeEvent::default(),
            bytes: 0,
            ack: Ack::Counter(counter.clone()),
        };
        envelope.ack.ack().await.unwrap();
        envelope.ack.ack().await.unwrap();
        assert_eq!(counter.load(Ordering::Relaxed), 2);
    }
}
