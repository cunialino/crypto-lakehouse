//! Local E2E helper: reads back the batcher's Iceberg table through the REST catalog and
//! prints snapshots, data-file layout, row counts and a few sample rows.
//!
//! ```sh
//! GARAGE_ACCESS_KEY=... GARAGE_SECRET_KEY=... \
//!   cargo run -p crypto-batcher --example read_trades
//! ```

use std::collections::HashMap;

use anyhow::{Context, Result};
use arrow_array::{
    Array, BooleanArray, Float64Array, Int64Array, RecordBatch, StringArray,
    TimestampMicrosecondArray,
};
use futures::TryStreamExt;
use iceberg::io::{
    S3_ACCESS_KEY_ID, S3_ENDPOINT, S3_PATH_STYLE_ACCESS, S3_REGION, S3_SECRET_ACCESS_KEY,
};
use iceberg::table::Table;
use iceberg::{Catalog, CatalogBuilder, NamespaceIdent, TableIdent};
use iceberg_catalog_rest::{
    REST_CATALOG_PROP_URI, REST_CATALOG_PROP_WAREHOUSE, RestCatalogBuilder,
};
use iceberg_storage_opendal::OpenDalStorageFactory;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<()> {
    let uri =
        std::env::var("LAKEKEEPER_URI").unwrap_or_else(|_| "http://127.0.0.1:18181/catalog".into());
    let warehouse =
        std::env::var("LAKEKEEPER_WAREHOUSE").unwrap_or_else(|_| "crypto_lakehouse".into());
    let namespace = std::env::var("ICEBERG_NAMESPACE").unwrap_or_else(|_| "trades".into());
    let table_name = std::env::var("ICEBERG_TABLE").unwrap_or_else(|_| "trades_rust".into());

    let mut props = HashMap::new();
    props.insert(REST_CATALOG_PROP_URI.to_string(), uri.clone());
    props.insert(REST_CATALOG_PROP_WAREHOUSE.to_string(), warehouse.clone());
    props.insert(
        S3_ENDPOINT.to_string(),
        std::env::var("GARAGE_ENDPOINT").unwrap_or_else(|_| "http://127.0.0.1:19000".into()),
    );
    props.insert(
        S3_REGION.to_string(),
        std::env::var("GARAGE_REGION").unwrap_or_else(|_| "eu-lambronx-1".into()),
    );
    props.insert(
        S3_ACCESS_KEY_ID.to_string(),
        std::env::var("GARAGE_ACCESS_KEY").context("GARAGE_ACCESS_KEY not set")?,
    );
    props.insert(
        S3_SECRET_ACCESS_KEY.to_string(),
        std::env::var("GARAGE_SECRET_KEY").context("GARAGE_SECRET_KEY not set")?,
    );
    props.insert(S3_PATH_STYLE_ACCESS.to_string(), "true".into());

    let catalog = RestCatalogBuilder::default()
        .with_storage_factory(Arc::new(OpenDalStorageFactory::S3 {
            customized_credential_load: None,
        }))
        .load("lakekeeper", props)
        .await
        .context("failed to load REST catalog")?;

    let ident = TableIdent::new(NamespaceIdent::new(namespace), table_name);
    let table: Table = catalog
        .load_table(&ident)
        .await
        .with_context(|| format!("failed to load {ident}"))?;

    println!("table:    {ident}");
    println!("location: {}", table.metadata().location());
    println!("snapshots: {}", table.metadata().snapshots().count());
    if let Some(snap) = table.metadata().current_snapshot() {
        println!(
            "current snapshot id={} summary={:?}",
            snap.snapshot_id(),
            snap.summary()
        );
    }

    let paths: Vec<String> = table
        .scan()
        .build()?
        .plan_files()
        .await?
        .map_ok(|t| t.data_file_path().to_string())
        .try_collect()
        .await?;
    println!("data files: {}", paths.len());
    for path in paths.iter().take(10) {
        println!("  {path}");
    }

    let batches = table
        .scan()
        .build()?
        .to_arrow()
        .await?
        .try_collect::<Vec<_>>()
        .await?;
    let rows: usize = batches.iter().map(|b| b.num_rows()).sum();
    println!("rows read back: {rows}");

    for batch in batches.iter().take(1) {
        print_sample(batch);
    }
    Ok(())
}

fn print_sample(batch: &RecordBatch) {
    let col = |name: &str| batch.column_by_name(name).unwrap();
    let strings = |name: &str| {
        let a = col(name).as_any().downcast_ref::<StringArray>().unwrap();
        (0..a.len().min(3))
            .map(|i| a.value(i).to_string())
            .collect::<Vec<_>>()
    };
    let longs = |name: &str| {
        let a = col(name).as_any().downcast_ref::<Int64Array>().unwrap();
        (0..a.len().min(3)).map(|i| a.value(i)).collect::<Vec<_>>()
    };
    let doubles = |name: &str| {
        let a = col(name).as_any().downcast_ref::<Float64Array>().unwrap();
        (0..a.len().min(3)).map(|i| a.value(i)).collect::<Vec<_>>()
    };
    let bools = |name: &str| {
        let a = col(name).as_any().downcast_ref::<BooleanArray>().unwrap();
        (0..a.len().min(3)).map(|i| a.value(i)).collect::<Vec<_>>()
    };
    let ts = || {
        let a = col("trade_ts")
            .as_any()
            .downcast_ref::<TimestampMicrosecondArray>()
            .unwrap();
        (0..a.len().min(3)).map(|i| a.value(i)).collect::<Vec<_>>()
    };

    println!("sample rows (up to 3):");
    println!("  exchange      {:?}", strings("exchange"));
    println!("  symbol        {:?}", strings("symbol"));
    println!("  trade_id      {:?}", longs("trade_id"));
    println!("  price         {:?}", doubles("price"));
    println!("  quantity      {:?}", doubles("quantity"));
    println!("  trade_time    {:?}", longs("trade_time"));
    println!(
        "  trade_ts      {:?} tz={:?}",
        ts(),
        col("trade_ts").data_type()
    );
    println!("  is_buyer_maker {:?}", bools("is_buyer_maker"));
}
