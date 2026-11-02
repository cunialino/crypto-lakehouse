//! Micro-batching trades from NATS JetStream into Apache Iceberg.
//!
//! - [`config`] — env-driven configuration
//! - [`schema`] — the `trades` Iceberg/Arrow schema and the row→`RecordBatch` conversion
//! - [`source`] — JetStream pull consumer producing [`source::Envelope`]s

pub mod config;
pub mod schema;
pub mod source;
