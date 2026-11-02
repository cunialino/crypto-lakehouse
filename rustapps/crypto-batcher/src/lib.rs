//! Micro-batching trades from NATS JetStream into Apache Iceberg.
//!
//! - [`config`] — env-driven configuration
//! - [`schema`] — the `trades` Iceberg/Arrow schema and the row→`RecordBatch` conversion

pub mod config;
pub mod schema;
