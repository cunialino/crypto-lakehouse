//! Micro-batching trades from NATS JetStream into Apache Iceberg.
//!
//! Pipeline shape (one binary, no external stream processor):
//!
//! ```text
//! JetStream pull consumer ──decode──► Accumulator ──commit──► Iceberg snapshot
//!        (offset)                    (rows/bytes/age)          then ack (at-least-once)
//! ```
//!
//! - [`config`]  — env-driven configuration
//! - [`schema`]  — the `trades` Iceberg/Arrow schema and the row→`RecordBatch` conversion
//! - [`source`]  — JetStream pull consumer producing [`source::Envelope`]s
//! - [`batcher`] — the flush policy + run loop
//! - [`sink`]    — parquet writing + `fast_append` commit against a REST catalog

pub mod batcher;
pub mod config;
pub mod schema;
pub mod sink;
pub mod source;
