//! The micro-batching core: accumulate rows, flush on size/bytes/age, commit, then ack.
//!
//! This module is deliberately free of Iceberg types (only `Envelope`/`Ack` come in from
//! `source`), which is what lets the whole flush policy be unit-tested with no
//! infrastructure at all.

use std::collections::HashSet;
use std::future::Future;
use std::time::{Duration, Instant};

use futures::StreamExt;

use crate::config::BatchConfig;
use crate::schema::TradeEvent;
use crate::sink::{IcebergSink, WriteStats};
use crate::source::{Ack, Envelope, TradeReceiver};

/// Why a micro-batch was closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlushTrigger {
    /// `BATCH_MAX_RECORDS` reached.
    Count,
    /// `BATCH_MAX_BYTES` reached.
    Bytes,
    /// Oldest buffered row got older than `BATCH_FLUSH_INTERVAL_SECS`.
    Interval,
    /// SIGTERM/SIGINT drain.
    Shutdown,
}

/// One group of rows that becomes exactly one Iceberg snapshot.
pub struct MicroBatch {
    pub rows: Vec<TradeEvent>,
    pub acks: Vec<Ack>,
    pub trigger: FlushTrigger,
    /// Rows dropped as `(exchange, symbol, trade_id)` duplicates of this batch.
    pub duplicates_dropped: usize,
    pub buffered_bytes: usize,
}

impl MicroBatch {
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

/// Buffers rows and decides when a micro-batch is complete.
pub struct Accumulator {
    policy: BatchConfig,
    rows: Vec<TradeEvent>,
    acks: Vec<Ack>,
    seen_keys: HashSet<(String, String, i64)>,
    bytes: usize,
    duplicates: usize,
    first_seen: Option<Instant>,
}

impl Accumulator {
    pub fn new(policy: BatchConfig) -> Self {
        Self {
            policy,
            rows: Vec::new(),
            acks: Vec::new(),
            seen_keys: HashSet::new(),
            bytes: 0,
            duplicates: 0,
            first_seen: None,
        }
    }

    /// Adds a row; returns `true` when the batch should be flushed right now.
    pub fn push(&mut self, envelope: Envelope) -> bool {
        if self.policy.dedup {
            let key = (
                envelope.event.exchange.clone(),
                envelope.event.symbol.clone(),
                envelope.event.trade_id as i64,
            );
            if !self.seen_keys.insert(key) {
                self.duplicates += 1;
                // The row is discarded, but it *was* delivered: still ack it, or the
                // source redelivers a row we intentionally dropped forever.
                self.acks.push(envelope.ack);
                return self.due(Instant::now());
            }
        }

        if self.rows.is_empty() {
            self.first_seen = Some(Instant::now());
        }
        self.bytes += envelope.bytes.max(1);
        self.rows.push(envelope.event);
        self.acks.push(envelope.ack);

        self.due(Instant::now())
    }

    /// True when a size/byte threshold is hit or the oldest row exceeded the age limit.
    pub fn due(&self, now: Instant) -> bool {
        self.trigger_now(now).is_some()
    }

    /// Which trigger would fire *right now*, priority order count > bytes > interval.
    pub fn trigger_now(&self, now: Instant) -> Option<FlushTrigger> {
        if self.is_empty() {
            return None;
        }
        if self.rows.len() >= self.policy.max_records {
            Some(FlushTrigger::Count)
        } else if self.bytes >= self.policy.max_bytes {
            Some(FlushTrigger::Bytes)
        } else if self.age_exceeded(now) {
            Some(FlushTrigger::Interval)
        } else {
            None
        }
    }

    fn age_exceeded(&self, now: Instant) -> bool {
        self.time_to_flush(now).is_some_and(|left| left.is_zero())
    }

    /// Time left before the age trigger fires (`None` when nothing is buffered).
    pub fn time_to_flush(&self, now: Instant) -> Option<Duration> {
        self.first_seen.map(|t| {
            let elapsed = now.saturating_duration_since(t);
            self.policy.flush_interval.saturating_sub(elapsed)
        })
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Detaches the buffered batch and resets the accumulator.
    pub fn take(&mut self, trigger: FlushTrigger) -> MicroBatch {
        self.first_seen = None;
        MicroBatch {
            rows: std::mem::take(&mut self.rows),
            acks: std::mem::take(&mut self.acks),
            duplicates_dropped: std::mem::take(&mut self.duplicates),
            buffered_bytes: std::mem::take(&mut self.bytes),
            trigger,
        }
    }
}

/// Cumulative process counters, logged on every flush.
#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    pub batches: u64,
    pub rows: u64,
    pub files: u64,
    pub bytes: u64,
    pub duplicates_dropped: u64,
    pub failed_commits: u64,
    pub ack_failures: u64,
}

/// Runs the batch loop until the source ends or `shutdown` fires.
///
/// On shutdown the buffered remainder is flushed before returning, so a rolling restart
/// loses nothing that was already read. A commit failure never acks its batch: the rows
/// come back through JetStream redelivery (at-least-once).
pub async fn run<F: Future<Output = ()>>(
    mut source: TradeReceiver,
    sink: &mut IcebergSink,
    policy: BatchConfig,
    shutdown: F,
) -> anyhow::Result<Stats> {
    let mut acc = Accumulator::new(policy);
    let mut stats = Stats::default();
    tokio::pin!(shutdown);

    loop {
        // Wait for the next row, the age trigger, or shutdown.
        let until_flush = acc.time_to_flush(Instant::now());
        let outcome = tokio::select! {
            envelope = source.recv() => Step::Row(envelope.map(Box::new)),
            _ = tokio::time::sleep(until_flush.unwrap_or(Duration::from_secs(3600))),
                if until_flush.is_some() => Step::Due,
            _ = &mut shutdown => Step::Stop,
        };

        match outcome {
            Step::Row(Some(envelope)) => {
                if acc.push(*envelope) {
                    let trigger = acc
                        .trigger_now(Instant::now())
                        .unwrap_or(FlushTrigger::Bytes);
                    flush(&mut acc, sink, &mut stats, trigger).await;
                }
            }
            // Channel closed: drain whatever is left and leave.
            Step::Row(None) => {
                flush(&mut acc, sink, &mut stats, FlushTrigger::Shutdown).await;
                return Ok(stats);
            }
            Step::Due => {
                flush(&mut acc, sink, &mut stats, FlushTrigger::Interval).await;
            }
            Step::Stop => {
                flush(&mut acc, sink, &mut stats, FlushTrigger::Shutdown).await;
                return Ok(stats);
            }
        }
    }
}

enum Step {
    Row(Option<Box<Envelope>>),
    Due,
    Stop,
}

/// Commit one micro-batch, then ack it. Never acks a batch whose commit failed.
pub async fn flush(
    acc: &mut Accumulator,
    sink: &mut IcebergSink,
    stats: &mut Stats,
    trigger: FlushTrigger,
) -> Option<WriteStats> {
    let batch = acc.take(trigger);
    if batch.is_empty() {
        return None;
    }

    stats.duplicates_dropped += batch.duplicates_dropped as u64;

    let written = match sink.write_batch(&batch.rows).await {
        Ok(written) => written,
        Err(err) => {
            stats.failed_commits += 1;
            // No ack here: JetStream redelivers after `ack_wait` and the rows are rewritten
            // into a fresh snapshot. Duplicates are handled downstream, as with Arroyo.
            tracing::error!(
                rows = batch.rows.len(),
                trigger = ?batch.trigger,
                error = ?err,
                "micro-batch commit failed; leaving messages unacked for redelivery"
            );
            // Throttle the fetch→commit→fail cycle so a catalog/S3 outage is not a hot loop.
            tokio::time::sleep(Duration::from_secs(1)).await;
            return None;
        }
    };

    let ack_failures = futures::stream::iter(batch.acks)
        .map(|ack| async move { ack.ack().await })
        .buffer_unordered(64)
        .filter_map(|res| async move { res.err() })
        .count()
        .await;

    stats.batches += 1;
    stats.rows += written.rows as u64;
    stats.files += written.files as u64;
    stats.bytes += written.bytes;
    stats.ack_failures += ack_failures as u64;

    tracing::info!(
        trigger = ?batch.trigger,
        rows = written.rows,
        files = written.files,
        partitions = written.partitions,
        bytes = written.bytes,
        write_ms = written.write_elapsed.as_millis() as u64,
        commit_ms = written.commit_elapsed.as_millis() as u64,
        duplicates_dropped = batch.duplicates_dropped,
        ack_failures,
        total_batches = stats.batches,
        total_rows = stats.rows,
        "flushed micro-batch to iceberg"
    );
    Some(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> BatchConfig {
        BatchConfig {
            max_records: 3,
            max_bytes: 1000,
            flush_interval: Duration::from_millis(50),
            dedup: true,
        }
    }

    fn row(trade_id: u64) -> Envelope {
        Envelope {
            event: TradeEvent {
                event_time: 1,
                symbol: "BTCUSDT".into(),
                exchange: "BINANCE".into(),
                trade_id,
                price: 1.0,
                quantity: 1.0,
                trade_time: 1,
                is_buyer_maker: false,
                is_best_price_match: true,
            },
            bytes: 10,
            ack: Ack::None,
        }
    }

    #[test]
    fn flushes_on_record_count() {
        let mut acc = Accumulator::new(policy());
        assert!(!acc.push(row(1)));
        assert!(!acc.push(row(2)));
        assert!(acc.push(row(3)), "third row reaches max_records");
        assert_eq!(acc.trigger_now(Instant::now()), Some(FlushTrigger::Count));

        let batch = acc.take(FlushTrigger::Count);
        assert_eq!(batch.rows.len(), 3);
        assert_eq!(batch.buffered_bytes, 30);
        assert!(acc.is_empty());
        assert!(acc.time_to_flush(Instant::now()).is_none());
    }

    #[test]
    fn flushes_on_bytes() {
        let mut acc = Accumulator::new(BatchConfig {
            max_records: 1_000,
            max_bytes: 25,
            ..policy()
        });
        assert!(!acc.push(row(1)));
        assert!(!acc.push(row(2)));
        assert!(acc.push(row(3)));
        assert_eq!(acc.trigger_now(Instant::now()), Some(FlushTrigger::Bytes));
    }

    #[test]
    fn age_trigger_uses_flush_interval() {
        let mut acc = Accumulator::new(BatchConfig {
            max_records: 1_000,
            flush_interval: Duration::from_millis(20),
            ..policy()
        });
        acc.push(row(1));
        assert!(!acc.due(Instant::now()));
        std::thread::sleep(Duration::from_millis(30));
        assert!(acc.due(Instant::now()));
        assert_eq!(
            acc.trigger_now(Instant::now()),
            Some(FlushTrigger::Interval)
        );
    }

    #[test]
    fn dedup_drops_same_trade_id_within_batch_but_keeps_acks() {
        let mut acc = Accumulator::new(policy());
        acc.push(row(7));
        acc.push(row(7));
        acc.push(row(8));
        let batch = acc.take(FlushTrigger::Count);
        assert_eq!(batch.rows.len(), 2, "duplicate row is not written");
        assert_eq!(batch.duplicates_dropped, 1);
        assert_eq!(
            batch.acks.len(),
            3,
            "the duplicate is still acknowledged so it stops being redelivered"
        );
    }

    #[test]
    fn dedup_is_scoped_per_exchange_and_symbol() {
        let mut acc = Accumulator::new(policy());
        let mut other = row(7);
        other.event.symbol = "ETHUSDT".into();
        acc.push(row(7));
        acc.push(other);
        let batch = acc.take(FlushTrigger::Count);
        assert_eq!(batch.rows.len(), 2);
        assert_eq!(batch.duplicates_dropped, 0);
    }

    #[test]
    fn dedup_disabled_keeps_everything() {
        let mut acc = Accumulator::new(BatchConfig {
            dedup: false,
            ..policy()
        });
        acc.push(row(7));
        acc.push(row(7));
        let batch = acc.take(FlushTrigger::Interval);
        assert_eq!(batch.rows.len(), 2);
        assert_eq!(batch.duplicates_dropped, 0);
    }
}
