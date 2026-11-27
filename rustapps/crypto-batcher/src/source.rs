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
use async_nats::jetstream::consumer::pull::Batch;
use async_nats::jetstream::consumer::{self, AckPolicy, DeliverPolicy, PullConsumer};
use async_nats::jetstream::stream::Stream;
use async_nats::jetstream::{self, Message};
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

/// Turns one JetStream message into an [`Envelope`]; undecodable payloads are acked here
/// so they leave the consumer instead of poisoning the queue; those yield `None`.
async fn process_message(msg: Message) -> Option<Envelope> {
    let bytes = msg.payload.len();
    match decode(&msg.payload) {
        Some(event) => Some(Envelope {
            event,
            bytes,
            ack: Ack::Nats(Box::new(msg)),
        }),
        None => {
            tracing::warn!(bytes, "undecodable payload; dropping (bad_data=drop)");
            if let Err(e) = msg.ack().await {
                tracing::error!("failed to ack undecodable payload: {e}");
            }
            None
        }
    }
}

enum BatchOutcome {
    Empty,
    Drained,
    Stopped,
}

async fn drain_batch(batch_stream: &mut Batch, tx: &mpsc::Sender<Envelope>) -> BatchOutcome {
    let mut pulled = false;
    while let Some(msg) = batch_stream.next().await {
        match msg {
            Ok(msg) => {
                pulled = true;
                if let Some(envelope) = process_message(msg).await
                    && tx.send(envelope).await.is_err()
                {
                    tracing::info!("batcher gone; nats source stopping");
                    return BatchOutcome::Stopped;
                }
            }
            Err(e) => {
                tracing::error!("fetch stream error: {e}");
                break;
            }
        }
    }
    if pulled {
        BatchOutcome::Drained
    } else {
        BatchOutcome::Empty
    }
}

/// Spawns the JetStream pull loop and returns the receiving end plus the micro-batching
/// policy the consumer actually allows.
///
/// The consumer is durable and explicit-ack: its ack floor *is* the pipeline offset, so
/// there is no separate state store to keep in sync. Its config comes from the `Consumer`
/// CR, and [`fit_batch_policy`] caps the configured policy to what that consumer can hold
/// unacked — so the batch loop must run on the returned [`BatchConfig`], not the one passed
/// in.
pub async fn spawn_nats_source(
    cfg: &crate::config::NatsConfig,
    batch: crate::config::BatchConfig,
) -> Result<(TradeReceiver, crate::config::BatchConfig)> {
    let client = async_nats::connect(&cfg.url)
        .await
        .with_context(|| format!("failed to connect to NATS at {}", cfg.url))?;
    let js = jetstream::new(client);

    let stream: Stream = js
        .get_stream(&cfg.stream)
        .await
        .with_context(|| format!("JetStream stream '{}' not found", cfg.stream))?;

    let (consumer, fitted) = load_consumer(&stream, cfg, &batch).await?;
    tracing::info!(
        stream = %cfg.stream,
        consumer = %cfg.consumer,
        filter = %cfg.subject_filter,
        fetch_batch = fitted.fetch_batch,
        "nats source started"
    );

    // Backpressure: at most one full micro-batch in flight towards the batcher.
    let (tx, rx) = mpsc::channel(fitted.batch.max_records.max(1));
    let fetch_batch = fitted.fetch_batch.max(1);
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
                Ok(mut batch_stream) => match drain_batch(&mut batch_stream, &tx).await {
                    BatchOutcome::Empty => {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    BatchOutcome::Drained => {}
                    BatchOutcome::Stopped => return,
                },
                Err(e) => {
                    tracing::error!("pull fetch failed: {e}; retrying in 1s");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    });

    Ok((TradeReceiver { rx }, fitted.batch))
}

/// The shortest micro-batch worth committing. Below this the writer degenerates towards one
/// snapshot per row, so a consumer that tight is not this pipeline's transport.
const MIN_FLUSH_INTERVAL: Duration = Duration::from_secs(30);
const MIN_BATCH_RECORDS: usize = 1_000;

/// The smallest `ack_wait` a batching policy can live with.
///
/// A buffered-but-unacked row is redelivered once `ack_wait` elapses, so it has to stay
/// well clear of `flush_interval`: otherwise the batcher is handed duplicates of rows it is
/// *still holding* and commits them a second time on purpose.
pub fn minimum_ack_wait(flush_interval: Duration) -> Duration {
    flush_interval * 4 + Duration::from_secs(10)
}

/// The other direction of [`minimum_ack_wait`]: the oldest a micro-batch may get for a
/// given redelivery timer.
fn maximum_flush_interval(ack_wait: Duration) -> Duration {
    // Whole seconds, so the cap in the warning is the cap actually applied.
    Duration::from_secs(ack_wait.saturating_sub(Duration::from_secs(10)).as_secs() / 4)
}

/// Rows that may sit unacked at once, halved so a buffered micro-batch and the fetch
/// filling it both fit. `max_ack_pending` <= 0 is unlimited.
fn max_records_within_ack_budget(max_ack_pending: i64) -> usize {
    if max_ack_pending <= 0 {
        usize::MAX
    } else {
        (max_ack_pending / 2) as usize
    }
}

/// The configured batching policy as the consumer permits it.
#[derive(Debug, Clone, PartialEq)]
pub struct FittedPolicy {
    pub batch: crate::config::BatchConfig,
    /// Messages per pull request, capped by the same ack budget as `batch`.
    pub fetch_batch: usize,
    /// Caps the consumer forced on the ConfigMap; empty when the configured policy fits.
    pub adjustments: Vec<String>,
}

/// Caps the configured batching policy to what the consumer can hold unacked.
///
/// `ackWait`/`maxAckPending` are declared in consumer.yaml, `BATCH_*` in the ConfigMap, and
/// the first bound the second: a micro-batch older than `ack_wait` is redelivered while the
/// batcher still holds it (and committed twice), and one larger than `max_ack_pending` is
/// never delivered at all. Shrinking the micro-batch fixes both, and shrinking is always
/// safe — more, smaller commits — so a tight consumer costs file counts rather than a
/// stopped pipeline. Every cap lands in `adjustments` because it does change freshness and
/// commit volume, which the ConfigMap otherwise states as fact.
///
/// What no shrink can fix is fatal: the settings in [`contract_problems`], plus an ack
/// budget so small the writer would degenerate (below [`MIN_FLUSH_INTERVAL`] or
/// [`MIN_BATCH_RECORDS`]).
pub fn fit_batch_policy(
    name: &str,
    actual: &consumer::Config,
    cfg: &crate::config::NatsConfig,
    configured: &crate::config::BatchConfig,
) -> Result<FittedPolicy, Vec<String>> {
    let mut problems = contract_problems(name, actual, cfg);
    let mut batch = configured.clone();
    let mut fetch_batch = cfg.fetch_batch.max(1);
    let mut adjustments = Vec::new();

    let flush_limit = maximum_flush_interval(actual.ack_wait);
    if flush_limit < MIN_FLUSH_INTERVAL {
        problems.push(format!(
            "{name} has ack_wait={}s, too short to buffer anything useful; the shortest \
             micro-batch worth committing is {}s, so consumer.yaml needs ackWait >= {}s",
            actual.ack_wait.as_secs(),
            MIN_FLUSH_INTERVAL.as_secs(),
            minimum_ack_wait(MIN_FLUSH_INTERVAL).as_secs(),
        ));
    } else if batch.flush_interval > flush_limit {
        adjustments.push(format!(
            "{name} ack_wait={}s caps BATCH_FLUSH_INTERVAL_SECS ({}s) at {}s: micro-batches \
             are smaller and committed more often than the ConfigMap says",
            actual.ack_wait.as_secs(),
            batch.flush_interval.as_secs(),
            flush_limit.as_secs(),
        ));
        batch.flush_interval = flush_limit;
    }

    let record_limit = max_records_within_ack_budget(actual.max_ack_pending);
    if record_limit < MIN_BATCH_RECORDS {
        problems.push(format!(
            "{name} has max_ack_pending={}, which cannot hold the {} rows a useful \
             micro-batch needs; consumer.yaml needs maxAckPending >= {}",
            actual.max_ack_pending,
            MIN_BATCH_RECORDS,
            MIN_BATCH_RECORDS as i64 * 2,
        ));
    } else {
        if batch.max_records > record_limit {
            adjustments.push(format!(
                "{name} max_ack_pending={} caps BATCH_MAX_RECORDS ({}) at {record_limit}",
                actual.max_ack_pending, batch.max_records,
            ));
            batch.max_records = record_limit;
        }
        if fetch_batch > record_limit {
            adjustments.push(format!(
                "{name} max_ack_pending={} caps BATCHER_FETCH_BATCH ({fetch_batch}) at \
                 {record_limit}",
                actual.max_ack_pending,
            ));
            fetch_batch = record_limit;
        }
    }

    if !problems.is_empty() {
        return Err(problems);
    }
    Ok(FittedPolicy {
        batch,
        fetch_batch,
        adjustments,
    })
}

/// Consumer settings that no batching policy can compensate for: each one loses or
/// double-counts trades regardless of batch sizes, so they fail the start.
fn contract_problems(
    name: &str,
    actual: &consumer::Config,
    cfg: &crate::config::NatsConfig,
) -> Vec<String> {
    let mut problems = Vec::new();

    if actual.deliver_subject.is_some() {
        problems.push(format!(
            "{name} is a push consumer (deliverSubject={:?}); the batcher pulls its own \
             batches, so consumer.yaml must not set deliverSubject",
            actual.deliver_subject
        ));
    }
    if actual.ack_policy != AckPolicy::Explicit {
        problems.push(format!(
            "{name} has ack_policy={:?}; the pipeline offset *is* this consumer's ack floor, \
             so consumer.yaml needs ackPolicy: explicit",
            actual.ack_policy
        ));
    }
    if actual.max_deliver > 0 {
        problems.push(format!(
            "{name} has max_deliver={}, so a message is abandoned after that many attempts \
             and there is no dead-letter consumer to catch it: those trades would be lost; \
             consumer.yaml needs maxDeliver: -1",
            actual.max_deliver
        ));
    }

    // A filter narrower than what the batcher reads drops matching trades before they
    // ever reach the process, and nothing downstream would notice the gap.
    if !actual.filter_subject.is_empty() && actual.filter_subject != cfg.subject_filter {
        problems.push(format!(
            "{name} has filter_subject={:?} but TRADES_SUBJECT_FILTER={:?}, so matching \
             trades are never delivered; consumer.yaml filterSubject must equal it",
            actual.filter_subject, cfg.subject_filter
        ));
    }
    if !actual.filter_subjects.is_empty() && !actual.filter_subjects.contains(&cfg.subject_filter) {
        problems.push(format!(
            "{name} filters {:?}, which does not cover TRADES_SUBJECT_FILTER={:?}; \
             consumer.yaml filterSubjects would hide trades from the batcher",
            actual.filter_subjects, cfg.subject_filter
        ));
    }

    problems
}

/// The consumer to pull from, plus the batching policy it permits. It has to exist already:
/// the batcher never creates or edits its own cursor.
async fn load_consumer(
    stream: &Stream,
    cfg: &crate::config::NatsConfig,
    batch: &crate::config::BatchConfig,
) -> Result<(PullConsumer, FittedPolicy)> {
    // JetStream's error is a boxed trait object, which anyhow cannot wrap.
    let consumer = stream
        .get_consumer(&cfg.consumer)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))
        .with_context(|| {
            format!(
                "failed to read consumer '{}': apply \
                 clusters/homelab/apps/batcher/consumer.yaml, the batcher does not create \
                 its own cursor",
                cfg.consumer
            )
        })?;

    let info = stream
        .consumer_info(&cfg.consumer)
        .await
        .with_context(|| format!("failed to read info of consumer '{}'", cfg.consumer))?;

    let fitted = fit_batch_policy(&cfg.consumer, &info.config, cfg, batch).map_err(|problems| {
        anyhow::anyhow!(
            "consumer '{}' cannot carry this pipeline:\n  - {}",
            cfg.consumer,
            problems.join("\n  - ")
        )
    })?;
    for note in &fitted.adjustments {
        tracing::warn!("{note}");
    }

    // Not a contradiction: delivering from elsewhere is a deliberate replay lever, it
    // just means old stored trades are skipped rather than backfilled.
    if info.config.deliver_policy != DeliverPolicy::All {
        tracing::warn!(
            consumer = %cfg.consumer,
            deliver_policy = ?info.config.deliver_policy,
            "consumer does not deliver everything still in the stream"
        );
    }

    tracing::info!(
        consumer = %cfg.consumer,
        ack_wait_secs = info.config.ack_wait.as_secs(),
        max_ack_pending = info.config.max_ack_pending,
        max_records = fitted.batch.max_records,
        flush_interval_secs = fitted.batch.flush_interval.as_secs(),
        num_pending = info.num_pending,
        num_ack_pending = info.num_ack_pending,
        num_redelivered = info.num_redelivered,
        "consumer ready"
    );

    Ok((consumer, fitted))
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
    fn ack_wait_floor_keeps_clear_of_the_flush_interval() {
        // A buffered-but-unacked batch must never age out first: 4x + 10 s.
        assert_eq!(
            minimum_ack_wait(Duration::from_secs(5)),
            Duration::from_secs(30)
        );
        assert_eq!(
            minimum_ack_wait(Duration::from_secs(900)),
            Duration::from_secs(3_610)
        );
    }

    /// The live batching policy (`batcher_config.yaml`), i.e. the one whose invariants the
    /// declared consumer has to satisfy.
    fn policy() -> crate::config::BatchConfig {
        crate::config::BatchConfig {
            max_records: 50_000,
            max_bytes: 64 * 1024 * 1024,
            flush_interval: Duration::from_secs(900),
            dedup: true,
        }
    }

    fn nats() -> crate::config::NatsConfig {
        crate::config::NatsConfig {
            url: "nats://127.0.0.1:4222".into(),
            stream: "tradesstream".into(),
            subject_filter: "exchange.*".into(),
            consumer: "iceberg-batcher".into(),
            fetch_batch: 2_000,
            fetch_expires: Duration::from_secs(2),
        }
    }

    /// The consumer as `clusters/homelab/apps/batcher/consumer.yaml` declares it.
    fn declared() -> consumer::Config {
        consumer::Config {
            ack_policy: AckPolicy::Explicit,
            deliver_policy: DeliverPolicy::All,
            ack_wait: Duration::from_secs(3_900),
            max_deliver: -1,
            max_ack_pending: 100_000,
            filter_subject: "exchange.*".into(),
            ..Default::default()
        }
    }

    fn fit(actual: consumer::Config) -> Result<FittedPolicy, Vec<String>> {
        fit_batch_policy("iceberg-batcher", &actual, &nats(), &policy())
    }

    fn reject(actual: consumer::Config) -> String {
        fit(actual)
            .expect_err("expected the consumer to be refused")
            .join("\n")
    }

    #[test]
    fn declared_consumer_needs_no_adjustments() {
        let fitted = fit(declared()).expect("consumer.yaml shape is accepted");
        assert_eq!(fitted.batch, policy(), "policy passes through unchanged");
        assert_eq!(fitted.fetch_batch, 2_000);
        assert!(fitted.adjustments.is_empty());
    }

    #[test]
    fn rejects_a_consumer_that_does_not_ack_each_message() {
        let mut config = declared();
        config.ack_policy = AckPolicy::All;
        assert!(reject(config).contains("ackPolicy"));
    }

    #[test]
    fn rejects_a_push_consumer() {
        let mut config = declared();
        config.deliver_subject = Some("trades.in".into());
        assert!(reject(config).contains("pull"));
    }

    #[test]
    fn rejects_a_consumer_that_stops_redelivering() {
        let mut config = declared();
        config.max_deliver = 10;
        assert!(reject(config).contains("maxDeliver"));
    }

    #[test]
    fn rejects_a_filter_that_hides_exchanges() {
        let mut config = declared();
        config.filter_subject = "exchange.BINANCE".into();
        assert!(reject(config).contains("filterSubject"));

        // No filter at all means the whole stream, which covers `exchange.*`.
        let mut config = declared();
        config.filter_subject = String::new();
        fit(config).expect("unfiltered consumer is accepted");
    }

    #[test]
    fn rejects_an_ack_wait_that_cannot_buffer_anything_useful() {
        let mut config = declared();
        config.ack_wait = Duration::from_secs(60);
        let err = reject(config);
        assert!(err.contains("ackWait"), "{err}");
        assert!(err.contains("130"), "the message names the minimum: {err}");
    }

    #[test]
    fn rejects_an_unacked_budget_that_cannot_hold_a_micro_batch() {
        let mut config = declared();
        config.max_ack_pending = 100;
        assert!(reject(config).contains("maxAckPending"));
    }

    #[test]
    fn shortens_the_flush_interval_to_fit_the_redelivery_timer() {
        let mut config = declared();
        config.ack_wait = Duration::from_secs(600);
        let ack_wait = config.ack_wait;
        let fitted = fit(config).expect("a tight ack_wait shrinks the batch, not the pipeline");

        assert!(
            minimum_ack_wait(fitted.batch.flush_interval) <= ack_wait,
            "the capped interval can no longer be outrun by ack_wait"
        );
        assert_eq!(fitted.batch.flush_interval, Duration::from_secs(147));
        assert_eq!(fitted.adjustments.len(), 1, "{:?}", fitted.adjustments);
        assert!(fitted.adjustments[0].contains("BATCH_FLUSH_INTERVAL_SECS"));
    }

    #[test]
    fn caps_batch_and_fetch_to_the_unacked_budget() {
        let mut config = declared();
        config.max_ack_pending = 5_000;
        let fitted = fit(config).expect("fits by shrinking");
        assert_eq!(fitted.batch.max_records, 2_500);
        assert_eq!(fitted.fetch_batch, 2_000, "already inside the budget");

        let mut config = declared();
        config.max_ack_pending = 2_000;
        let fitted = fit(config).expect("fits by shrinking");
        assert_eq!(fitted.batch.max_records, 1_000, "exactly the floor");
        assert_eq!(fitted.fetch_batch, 1_000);
        assert_eq!(fitted.adjustments.len(), 2, "{:?}", fitted.adjustments);
    }

    #[test]
    fn an_unlimited_ack_budget_needs_no_shrinking() {
        let mut config = declared();
        config.max_ack_pending = -1;
        let fitted = fit(config).expect("unlimited is headroom by definition");
        assert_eq!(fitted.batch, policy());
        assert!(fitted.adjustments.is_empty());
    }

    #[test]
    fn reports_every_problem_at_once() {
        let mut config = declared();
        config.ack_wait = Duration::from_secs(60);
        config.max_deliver = 5;
        let err = reject(config);
        assert!(
            err.contains("ackWait") && err.contains("maxDeliver"),
            "{err}"
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
