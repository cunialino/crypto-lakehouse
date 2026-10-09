//! Shared Lakekeeper + DataFusion plumbing for the Iceberg tools in this crate:
//! `crypto-iceberg-query` (per-day row counts) and `crypto-dedup` (duplicate audit /
//! rewrite).
//!
//! Configuration comes from the environment, using the same variable names as
//! `crypto-batcher`, so a CronJob pod can reuse `batcher-config` +
//! `garage-s3-credentials` verbatim instead of inventing a second spelling.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use datafusion::execution::SessionStateBuilder;
use datafusion::execution::disk_manager::{DiskManagerBuilder, DiskManagerMode};
use datafusion::execution::runtime_env::RuntimeEnvBuilder;
use datafusion::prelude::{SessionConfig, SessionContext};
use iceberg::Catalog;
use iceberg::CatalogBuilder;
use iceberg::io::{
    S3_ACCESS_KEY_ID, S3_ENDPOINT, S3_PATH_STYLE_ACCESS, S3_REGION, S3_SECRET_ACCESS_KEY,
};
use iceberg_catalog_rest::{
    REST_CATALOG_PROP_URI, REST_CATALOG_PROP_WAREHOUSE, RestCatalogBuilder,
};
use iceberg_datafusion::IcebergCatalogProvider;
use iceberg_storage_opendal::OpenDalStorageFactory;

/// Catalog name the Iceberg catalog is registered under (`lk.<namespace>.<table>`).
pub const CATALOG: &str = "lk";

/// The 11 data columns of the `trades` contract, in table order.
///
/// Kept here rather than derived at runtime so an `INSERT INTO` cannot silently write
/// columns in the wrong order — see `crypto_batcher::schema` for the field ids.
pub const TRADE_COLUMNS: &str = "event_time, symbol, exchange, trade_id, price, quantity, \
                                  trade_time, is_buyer_maker, is_best_price_match, trade_ts, \
                                  event_ts";

/// Memory / spill knobs for the bounded session.
#[derive(Debug, Clone)]
pub struct EngineOptions {
    /// DataFusion memory pool cap. This is an *accounting* limit, not an RSS limit.
    pub memory_mb: usize,
    /// Spill directory. `None` keeps DataFusion's default.
    pub temp_dir: Option<PathBuf>,
    /// Spill budget for that directory.
    pub max_temp_gib: u64,
    pub target_partitions: usize,
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            memory_mb: 2048,
            temp_dir: None,
            max_temp_gib: 64,
            target_partitions: 8,
        }
    }
}

/// Build a `SessionContext` with the Lakekeeper REST catalog registered as [`CATALOG`].
///
/// Why the pool is capped at all: unbounded DataFusion runs on this box have hit the
/// node's memory, and some operators (notably the final merge of `count(DISTINCT ...)`,
/// whose `group_values`/hashbrown allocations are not registered against the pool in
/// DataFusion 53.1) are only *partially* accounted. The cap plus spilling keeps the
/// common shapes honest; for the unaccounted ones it is a strong hint, not a hard limit,
/// which is why callers print [`peak_rss_mib`] rather than trust it.
pub async fn iceberg_session(opts: &EngineOptions) -> Result<SessionContext> {
    let uri = std::env::var("LAKEKEEPER_URI")
        .unwrap_or_else(|_| "http://127.0.0.1:8181/catalog".to_string());
    let warehouse =
        std::env::var("LAKEKEEPER_WAREHOUSE").unwrap_or_else(|_| "crypto_lakehouse".to_string());
    let s3_endpoint = std::env::var("GARAGE_ENDPOINT")
        .unwrap_or_else(|_| "http://garage-svc.garage.svc.cluster.local:3900".to_string());
    let s3_region = std::env::var("GARAGE_REGION").unwrap_or_else(|_| "eu-lambronx-1".to_string());
    // Secrets come from the environment (sourced from .env / a mounted secret), never
    // hardcoded and never logged.
    let access_key = std::env::var("GARAGE_ACCESS_KEY").context("GARAGE_ACCESS_KEY not set")?;
    let secret_key = std::env::var("GARAGE_SECRET_KEY").context("GARAGE_SECRET_KEY not set")?;

    let mut props = HashMap::new();
    props.insert(REST_CATALOG_PROP_URI.to_string(), uri);
    props.insert(REST_CATALOG_PROP_WAREHOUSE.to_string(), warehouse);
    props.insert(S3_ENDPOINT.to_string(), s3_endpoint);
    props.insert(S3_REGION.to_string(), s3_region);
    props.insert(S3_ACCESS_KEY_ID.to_string(), access_key);
    props.insert(S3_SECRET_ACCESS_KEY.to_string(), secret_key);
    props.insert(S3_PATH_STYLE_ACCESS.to_string(), "true".to_string());

    let storage_factory = Arc::new(OpenDalStorageFactory::S3 {
        customized_credential_load: None,
    });

    let catalog: Arc<dyn Catalog> = Arc::new(
        RestCatalogBuilder::default()
            .with_storage_factory(storage_factory)
            .load("lk", props)
            .await
            .context("failed to load REST catalog")?,
    );

    let provider = IcebergCatalogProvider::try_new(catalog)
        .await
        .context("failed to build Iceberg catalog provider")?;

    let mut runtime = RuntimeEnvBuilder::new()
        .with_memory_limit(opts.memory_mb * 1024 * 1024, 1.0)
        .with_max_temp_directory_size(opts.max_temp_gib * 1024 * 1024 * 1024);
    if let Some(dir) = &opts.temp_dir {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("failed to create spill dir {}", dir.display()))?;
        runtime = runtime.with_disk_manager_builder(
            DiskManagerBuilder::default()
                .with_mode(DiskManagerMode::Directories(vec![dir.clone()])),
        );
    }

    let state = SessionStateBuilder::new()
        .with_config(
            SessionConfig::new()
                .with_batch_size(8192)
                .with_target_partitions(opts.target_partitions),
        )
        .with_runtime_env(
            runtime
                .build_arc()
                .context("failed to build bounded RuntimeEnv")?,
        )
        .with_default_features()
        .build();

    let ctx = SessionContext::from(state);
    ctx.register_catalog(CATALOG, Arc::new(provider));
    Ok(ctx)
}

/// Reject identifiers we are about to splice into SQL text.
///
/// `&str` table names are unavoidable with `ctx.sql()`, so keep a strict allow-list
/// instead of pretending a quote-escape is enough.
pub fn checked_qualified_name(table: &str) -> Result<&str> {
    let ok = !table.is_empty()
        && table.matches('.').count() == 1
        && table.split('.').all(|part| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
        });
    anyhow::ensure!(
        ok,
        "invalid table reference {table:?}: expected `<namespace>.<table>`"
    );
    Ok(table)
}

/// Peak RSS of this process in MiB (`VmHWM`), or 0 where `/proc` is unavailable.
pub fn peak_rss_mib() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find(|line| line.starts_with("VmHWM:"))?
                .split_whitespace()
                .nth(1)?
                .parse::<u64>()
                .ok()
        })
        .map_or(0, |kib| kib / 1024)
}
