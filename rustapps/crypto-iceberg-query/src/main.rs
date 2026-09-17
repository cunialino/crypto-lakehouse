use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use datafusion::execution::SessionStateBuilder;
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

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let uri = std::env::var("LAKEKEEPER_URI")
        .unwrap_or_else(|_| "http://127.0.0.1:8181/catalog".to_string());
    let warehouse =
        std::env::var("LAKEKEEPER_WAREHOUSE").unwrap_or_else(|_| "crypto_lakehouse".to_string());
    let s3_endpoint = std::env::var("GARAGE_ENDPOINT")
        .unwrap_or_else(|_| "http://garage-svc.garage.svc.cluster.local:3900".to_string());
    let s3_region = std::env::var("GARAGE_REGION").unwrap_or_else(|_| "eu-lambronx-1".to_string());
    // Secrets come from the environment (sourced from .env), never hardcoded.
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

    let provider = IcebergCatalogProvider::try_new(catalog.clone())
        .await
        .context("failed to build Iceberg catalog provider")?;

    // Bound DataFusion memory instead of letting it grow unbounded:
    //  - count(DISTINCT concat(...)) over ~215M distinct keys builds large hash
    //    tables. Memory per partial hash table scales as distinct_keys / partitions,
    //    so MORE partitions = smaller tables = lower peak (8p -> ~3GB/table).
    //    Fewer partitions blow the pool in a single ~6GB allocation (observed
    //    "Failed to reserve memory for sort during spill" at 4p/8GB).
    //  - Cap the pool at 8GB; overflow spills to local NVMe (/tmp, 173G free).
    //    Slower than unbounded (~30GB) but memory-safe.
    // Bound DataFusion memory. count(DISTINCT) over ~215M distinct keys builds a
    // final-merge hash table that DataFusion 53.1.0 does NOT account against the
    // memory pool (row_hash group_values/hashbrown allocations are untracked), so
    // the pool cannot hard-cap RSS for this query:
    //   - unbounded:            ~30GB RSS, day-agg ~70s, completes
    //   - GreedyPool 8/16GB:    RSS ~25-29GB (accounting lag), completes ~80-91s
    //   - FairSpillPool 8-24GB: spills the table but fails reserving sort
    //     headroom for the spill (pool full of untracked table memory) -> error
    // A true hard cap below ~25GB is only possible at the OS level (cgroup).
    let config = SessionConfig::new()
        .with_batch_size(8192)
        .with_target_partitions(8);
    let runtime_env = RuntimeEnvBuilder::new()
        .with_memory_limit(16 * 1024 * 1024 * 1024, 1.0)
        .with_max_temp_directory_size(64 * 1024 * 1024 * 1024)
        .build_arc()
        .context("failed to build bounded RuntimeEnv")?;
    let state = SessionStateBuilder::new()
        .with_config(config)
        .with_runtime_env(runtime_env)
        .with_default_features()
        .build();
    let ctx = SessionContext::from(state);
    ctx.register_catalog("lk", Arc::new(provider));
    eprintln!("[progress] catalog registered",);
    drop(catalog);
    let day_sql = r#"
        SELECT
            to_char(trade_ts, '%Y%m%d') AS day,
            count(*) AS cnt,
            count(DISTINCT concat(exchange, cast(trade_id as varchar), symbol)) AS cnt_dist
        FROM lk.trades.trades
        WHERE trade_ts between TIMESTAMP '2026-08-01 00:00:00'
        AND TIMESTAMP '2026-08-18 00:00:00'
        GROUP BY day
        ORDER BY day
    "#;

    // Warm-up: plan only, no collect.
    let df = ctx.sql(day_sql).await.context("plan day query")?;
    eprintln!("[progress] day query planned");
    let _plan = df.clone().explain(false, false)?;

    let start = Instant::now();
    let df = ctx.sql(day_sql).await.context("run day query")?;
    eprintln!("[progress] day query re-planned, starting show");
    df.show().await.context("show day query")?;
    let elapsed = start.elapsed();
    eprintln!("[progress] day query shown");
    println!("day-agg elapsed: {:?}", elapsed);

    // Full scan count (no filter, all data) to measure raw scan throughput.
    let start = Instant::now();
    let df = ctx
        .sql("SELECT count(*) AS c FROM lk.trades.trades")
        .await
        .context("plan count query")?;
    df.show().await.context("show count query")?;
    let count_elapsed = start.elapsed();
    println!("count(*) elapsed: {:?}", count_elapsed);

    Ok(())
}
