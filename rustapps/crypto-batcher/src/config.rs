//! Runtime configuration for the Iceberg micro-batcher.
//!
//! Everything is env-driven (12-factor) so the same binary runs locally against the
//! `teststack/` podman stack and in-cluster, exactly like `crypto-iceberg-query` does.
//! Secrets are *only* read from the environment and never logged.

use std::time::Duration;

use anyhow::{Context, Result};

/// Where the batcher reads trades from.
#[derive(Debug, Clone)]
pub struct NatsConfig {
    /// `nats://host:4222`
    pub url: String,
    /// JetStream stream to read (subjects `exchange.*`).
    pub stream: String,
    /// Subject filter used by the durable consumer.
    pub subject_filter: String,
    /// Durable pull consumer name. Owns the ack offset => this is the batcher's "cursor".
    pub consumer: String,
    /// How long a fetched-but-unacked message may live before JetStream redelivers it.
    ///
    /// Must comfortably exceed `BatchConfig::flush_interval`, otherwise buffered messages
    /// get redelivered while they are still waiting to be committed.
    pub ack_wait: Duration,
    /// Max messages asked for in a single pull `fetch`.
    pub fetch_batch: usize,
    /// Long-poll time for a pull `fetch` when the stream is idle.
    pub fetch_expires: Duration,
}

/// Iceberg/Lakekeeper destination.
#[derive(Debug, Clone)]
pub struct IcebergConfig {
    /// Lakekeeper REST catalog uri, e.g. `http://127.0.0.1:18181/catalog`.
    pub uri: String,
    /// Lakekeeper warehouse (project-scoped) name.
    pub warehouse: String,
    /// Optional `client_id:secret` used for OAuth2 client-credentials against the catalog.
    pub credential: Option<String>,
    pub namespace: String,
    pub table: String,
    pub s3_endpoint: String,
    pub s3_region: String,
    pub s3_access_key_id: String,
    pub s3_secret_access_key: String,
    pub s3_path_style: bool,
    /// Parquet compression codec (`zstd` matches the retired Arroyo sink).
    pub parquet_compression: String,
    /// Target size of one parquet file; bigger micro-batches roll into several files.
    pub target_file_size: usize,
    /// Number of commit attempts (with exponential backoff) before giving up on a batch.
    ///
    /// Only the *metadata commit* is retried (with a metadata reload between attempts);
    /// parquet files of a given flush are written once.
    pub commit_retries: u32,
}

/// The micro-batching policy itself.
#[derive(Debug, Clone)]
pub struct BatchConfig {
    /// Flush once this many rows are buffered.
    pub max_records: usize,
    /// Flush once the buffered (encoded) payload exceeds this many bytes.
    pub max_bytes: usize,
    /// Flush this long after the *oldest* buffered row, so latency stays bounded even
    /// when the traffic never reaches `max_records`.
    pub flush_interval: Duration,
    /// Drop `(exchange, symbol, trade_id)` duplicates found inside the same micro-batch.
    ///
    /// Only intra-batch: JetStream redelivery after a crash can still replay a whole
    /// already-committed batch, which is deduped downstream (as it was with Arroyo).
    pub dedup: bool,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub nats: NatsConfig,
    pub iceberg: IcebergConfig,
    pub batch: BatchConfig,
}

impl Config {
    /// Builds the config from the environment, applying in-cluster-friendly defaults.
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            nats: NatsConfig {
                url: required("NATS_URL")?,
                stream: string("TRADES_STREAM", "tradesstream"),
                subject_filter: string("TRADES_SUBJECT_FILTER", "exchange.*"),
                consumer: string("BATCHER_CONSUMER", "iceberg-batcher"),
                ack_wait: seconds("BATCHER_ACK_WAIT_SECS", 300.),
                fetch_batch: usize_env("BATCHER_FETCH_BATCH", 2_000),
                fetch_expires: seconds("BATCHER_FETCH_EXPIRES_SECS", 2.),
            },
            iceberg: IcebergConfig {
                uri: string(
                    "LAKEKEEPER_URI",
                    "http://lakekeeper-lakekeeper.default.svc.cluster.local:8181/catalog",
                ),
                warehouse: string("LAKEKEEPER_WAREHOUSE", "crypto_lakehouse"),
                credential: optional("LAKEKEEPER_CREDENTIAL"),
                namespace: string("ICEBERG_NAMESPACE", "trades"),
                table: string("ICEBERG_TABLE", "trades_rust"),
                s3_endpoint: string(
                    "GARAGE_ENDPOINT",
                    "http://garage-svc.garage.svc.cluster.local:3900",
                ),
                s3_region: string("GARAGE_REGION", "eu-lambronx-1"),
                s3_access_key_id: required("GARAGE_ACCESS_KEY")?,
                s3_secret_access_key: required("GARAGE_SECRET_KEY")?,
                s3_path_style: bool_env("GARAGE_PATH_STYLE", true),
                parquet_compression: string("ICEBERG_PARQUET_COMPRESSION", "zstd"),
                target_file_size: bytes_env("ICEBERG_TARGET_FILE_SIZE", 128 * 1024 * 1024),
                commit_retries: u32_env("COMMIT_RETRIES", 5),
            },
            batch: BatchConfig {
                max_records: usize_env("BATCH_MAX_RECORDS", 50_000),
                max_bytes: bytes_env("BATCH_MAX_BYTES", 32 * 1024 * 1024),
                flush_interval: seconds("BATCH_FLUSH_INTERVAL_SECS", 30.),
                dedup: bool_env("BATCH_DEDUP", true),
            },
        })
    }
}

fn optional(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

fn required(key: &str) -> Result<String> {
    optional(key).with_context(|| format!("{key} not set"))
}

fn string(key: &str, default: &str) -> String {
    optional(key).unwrap_or_else(|| default.to_string())
}

fn parse<T: std::str::FromStr>(key: &str, default: T) -> T {
    match optional(key) {
        Some(raw) => raw.parse().unwrap_or_else(|_| {
            tracing::warn!(key, %raw, "unparsable value, using default");
            default
        }),
        None => default,
    }
}

fn usize_env(key: &str, default: usize) -> usize {
    parse(key, default)
}

fn u32_env(key: &str, default: u32) -> u32 {
    parse(key, default)
}

fn seconds(key: &str, default: f64) -> Duration {
    Duration::from_secs_f64(parse::<f64>(key, default))
}

/// Accepts a raw byte count (`134217728`) or a binary suffix (`128MiB`, `2GB`).
fn bytes_env(key: &str, default: usize) -> usize {
    match optional(key) {
        Some(raw) => parse_bytes(&raw).unwrap_or_else(|| {
            tracing::warn!(key, %raw, "unparsable byte size, using default");
            default
        }),
        None => default,
    }
}

fn parse_bytes(raw: &str) -> Option<usize> {
    let raw = raw.trim();
    let (num, mul) = match raw
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .filter(|_| !raw.starts_with('.'))
    {
        Some(idx) => raw.split_at(idx),
        None => (raw, ""),
    };
    let value: f64 = num.parse().ok()?;
    let factor: f64 = match mul.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1.,
        "k" | "kb" | "kib" => 1024.,
        "m" | "mb" | "mib" => 1024. * 1024.,
        "g" | "gb" | "gib" => 1024. * 1024. * 1024.,
        _ => return None,
    };
    Some((value * factor) as usize)
}

fn bool_env(key: &str, default: bool) -> bool {
    match optional(key) {
        Some(raw) => matches!(
            raw.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        None => default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_byte_sizes() {
        assert_eq!(parse_bytes("1024"), Some(1024));
        assert_eq!(parse_bytes("128MiB"), Some(128 * 1024 * 1024));
        assert_eq!(parse_bytes("2gb"), Some(2 * 1024 * 1024 * 1024));
        assert_eq!(parse_bytes("12x"), None);
    }
}
