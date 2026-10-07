//! The `trades` Iceberg schema, its partition spec, and the Arrow projection of a
//! micro-batch.
//!
//! The column set is intentionally identical to the retired Arroyo `iceberg_trades_sink`
//! connection table (`deploy/base/arroyo/tables/iceberg_trades_sink.json`) so that
//! downstream consumers (Spark maintenance job, `crypto-iceberg-query`, dbt) do not care
//! which writer produced the data:
//!
//! ```text
//!  1 event_time          long          (ms since epoch, exchange event time)
//!  2 symbol              string
//!  3 exchange            string
//!  4 trade_id            long
//!  5 price               double
//!  6 quantity            double
//!  7 trade_time          long          (ms since epoch, exchange trade time)
//!  8 is_buyer_maker      boolean
//!  9 is_best_price_match boolean
//! 10 trade_ts            timestamptz   (micros, derived from trade_time)
//! 11 event_ts            timestamptz   (micros, derived from event_time)
//! ```
//!
//! Partitioning mirrors Arroyo too: `identity(exchange)` + `day(trade_ts)`.
//!
//! The proto carries `uint64` millis; Iceberg has no unsigned long and the Arroyo sink
//! cast them to `bigint`, so the same wrapping `as i64` cast is used here.

use std::sync::Arc;

use anyhow::{Context, Result};
use arrow_array::builder::{
    BooleanBuilder, Float64Builder, Int64Builder, StringBuilder, TimestampMicrosecondBuilder,
};
use arrow_array::{ArrayRef, RecordBatch};
use arrow_schema::Schema as ArrowSchema;
use iceberg::arrow::schema_to_arrow_schema;
use iceberg::spec::{
    NestedField, PartitionSpec, PrimitiveType, Schema as IcebergSchema, Transform, Type,
    UnboundPartitionSpec,
};

pub use crypto_data::data::TradeEventProto as TradeEvent;

// Field ids are part of the Iceberg schema contract: never renumber them.
pub const ID_EVENT_TIME: i32 = 1;
pub const ID_SYMBOL: i32 = 2;
pub const ID_EXCHANGE: i32 = 3;
pub const ID_TRADE_ID: i32 = 4;
pub const ID_PRICE: i32 = 5;
pub const ID_QUANTITY: i32 = 6;
pub const ID_TRADE_TIME: i32 = 7;
pub const ID_IS_BUYER_MAKER: i32 = 8;
pub const ID_IS_BEST_PRICE_MATCH: i32 = 9;
pub const ID_TRADE_TS: i32 = 10;
pub const ID_EVENT_TS: i32 = 11;

/// Iceberg stores `timestamptz` as UTC; arrow-rs spells the UTC timezone `+00:00`
/// (that is exactly what `TimestampArray::with_timezone_utc()` produces), and the Arrow
/// schema derived from the Iceberg schema uses the same spelling.
pub const ICEBERG_TZ: &str = "+00:00";

/// Epoch milliseconds -> Iceberg/Arrow epoch microseconds.
///
/// Getting this wrong is the classic failure of this pipeline: `to_timestamp_micros`
/// on millisecond input parked every Arroyo row in the `1970-01-21` partition (see
/// `deploy/docs/arroyo-ingestion-guide.md`), so the factor is spelled out once here.
pub const MICROS_PER_MILLI: i64 = 1_000;

/// The Iceberg schema of the trades table (schema id 1, matching a freshly created table).
pub fn table_schema() -> Result<IcebergSchema> {
    IcebergSchema::builder()
        .with_schema_id(1)
        .with_identifier_field_ids(vec![ID_EXCHANGE, ID_SYMBOL, ID_TRADE_ID])
        .with_fields(vec![
            NestedField::required(
                ID_EVENT_TIME,
                "event_time",
                Type::Primitive(PrimitiveType::Long),
            )
            .into(),
            NestedField::required(ID_SYMBOL, "symbol", Type::Primitive(PrimitiveType::String))
                .into(),
            NestedField::required(
                ID_EXCHANGE,
                "exchange",
                Type::Primitive(PrimitiveType::String),
            )
            .into(),
            NestedField::required(
                ID_TRADE_ID,
                "trade_id",
                Type::Primitive(PrimitiveType::Long),
            )
            .into(),
            NestedField::required(ID_PRICE, "price", Type::Primitive(PrimitiveType::Double)).into(),
            NestedField::required(
                ID_QUANTITY,
                "quantity",
                Type::Primitive(PrimitiveType::Double),
            )
            .into(),
            NestedField::required(
                ID_TRADE_TIME,
                "trade_time",
                Type::Primitive(PrimitiveType::Long),
            )
            .into(),
            NestedField::required(
                ID_IS_BUYER_MAKER,
                "is_buyer_maker",
                Type::Primitive(PrimitiveType::Boolean),
            )
            .into(),
            NestedField::required(
                ID_IS_BEST_PRICE_MATCH,
                "is_best_price_match",
                Type::Primitive(PrimitiveType::Boolean),
            )
            .into(),
            NestedField::required(
                ID_TRADE_TS,
                "trade_ts",
                Type::Primitive(PrimitiveType::Timestamptz),
            )
            .into(),
            NestedField::required(
                ID_EVENT_TS,
                "event_ts",
                Type::Primitive(PrimitiveType::Timestamptz),
            )
            .into(),
        ])
        .build()
        .context("failed to build trades iceberg schema")
}

/// Unbound partition spec used when *creating* the table: `identity(exchange), day(trade_ts)`.
pub fn unbound_partition_spec() -> UnboundPartitionSpec {
    iceberg::spec::UnboundPartitionSpec::builder()
        .add_partition_field(ID_EXCHANGE, "exchange", Transform::Identity)
        .expect("identity(exchange) is a valid partition field")
        .add_partition_field(ID_TRADE_TS, "trade_ts_day", Transform::Day)
        .expect("day(trade_ts) is a valid partition field")
        .build()
}

/// Bound partition spec used when *writing* (transforms + partition paths).
pub fn partition_spec(schema: &Arc<IcebergSchema>) -> Result<PartitionSpec> {
    unbound_partition_spec()
        .bind(schema.clone())
        .context("failed to bind trades partition spec")
}

/// Arrow view of the Iceberg schema, carrying `PARQUET_FIELD_ID` metadata on every field
/// (required by iceberg's default `FieldMatchMode::Id`).
pub fn arrow_schema(schema: &IcebergSchema) -> Result<Arc<ArrowSchema>> {
    Ok(Arc::new(
        schema_to_arrow_schema(schema).context("failed to convert iceberg schema to arrow")?,
    ))
}

/// Turns a micro-batch of decoded trade events into one Arrow batch.
///
/// `rows` must be non-empty. Column order/nullability follow `arrow_schema`, which is
/// derived from the table schema, so the batch always matches the destination table.
pub fn to_record_batch(
    arrow_schema: &Arc<ArrowSchema>,
    rows: &[TradeEvent],
) -> Result<RecordBatch> {
    let n = rows.len();
    let mut event_time = Int64Builder::with_capacity(n);
    // ~16 bytes of utf8 per symbol is generous for `BTCUSDT`-style tickers.
    let mut symbol = StringBuilder::with_capacity(n, n * 16);
    let mut exchange = StringBuilder::with_capacity(n, n * 16);
    let mut trade_id = Int64Builder::with_capacity(n);
    let mut price = Float64Builder::with_capacity(n);
    let mut quantity = Float64Builder::with_capacity(n);
    let mut trade_time = Int64Builder::with_capacity(n);
    let mut is_buyer_maker = BooleanBuilder::with_capacity(n);
    let mut is_best_price_match = BooleanBuilder::with_capacity(n);
    let mut trade_ts = TimestampMicrosecondBuilder::with_capacity(n).with_timezone(ICEBERG_TZ);
    let mut event_ts = TimestampMicrosecondBuilder::with_capacity(n).with_timezone(ICEBERG_TZ);

    for row in rows {
        let trade_time_ms = row.trade_time as i64;
        let event_time_ms = row.event_time as i64;

        event_time.append_value(event_time_ms);
        symbol.append_value(&row.symbol);
        exchange.append_value(&row.exchange);
        trade_id.append_value(row.trade_id as i64);
        price.append_value(row.price);
        quantity.append_value(row.quantity);
        trade_time.append_value(trade_time_ms);
        is_buyer_maker.append_value(row.is_buyer_maker);
        is_best_price_match.append_value(row.is_best_price_match);
        trade_ts.append_value(trade_time_ms * MICROS_PER_MILLI);
        event_ts.append_value(event_time_ms * MICROS_PER_MILLI);
    }

    let columns: Vec<ArrayRef> = vec![
        Arc::new(event_time.finish()),
        Arc::new(symbol.finish()),
        Arc::new(exchange.finish()),
        Arc::new(trade_id.finish()),
        Arc::new(price.finish()),
        Arc::new(quantity.finish()),
        Arc::new(trade_time.finish()),
        Arc::new(is_buyer_maker.finish()),
        Arc::new(is_best_price_match.finish()),
        Arc::new(trade_ts.finish()),
        Arc::new(event_ts.finish()),
    ];

    RecordBatch::try_new(arrow_schema.clone(), columns)
        .context("failed to build arrow record batch")
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::TimestampMicrosecondArray;

    fn sample(millis: u64) -> TradeEvent {
        TradeEvent {
            event_time: millis,
            symbol: "BTCUSDT".into(),
            exchange: "BINANCE".into(),
            trade_id: 42,
            price: 1.5,
            quantity: 2.0,
            trade_time: millis,
            is_buyer_maker: true,
            is_best_price_match: true,
        }
    }

    #[test]
    fn schema_has_all_eleven_columns() {
        let schema = table_schema().unwrap();
        assert_eq!(schema.as_struct().fields().len(), 11);
        assert_eq!(schema.name_by_field_id(ID_TRADE_TS).unwrap(), "trade_ts");
    }

    #[test]
    fn partition_spec_is_exchange_plus_day() {
        let schema = Arc::new(table_schema().unwrap());
        let spec = partition_spec(&schema).unwrap();
        let names: Vec<&str> = spec.fields().iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["exchange", "trade_ts_day"]);
    }

    #[test]
    fn record_batch_matches_arrow_schema_and_converts_millis() {
        let schema = Arc::new(table_schema().unwrap());
        let arrow = arrow_schema(&schema).unwrap();
        // 2026-10-07T00:00:00.123Z in millis.
        let millis = 1_791_331_200_123_u64;
        let batch = to_record_batch(&arrow, &[sample(millis)]).unwrap();

        assert_eq!(batch.num_rows(), 1);
        assert_eq!(batch.schema(), arrow);

        let trade_ts = batch
            .column_by_name("trade_ts")
            .unwrap()
            .as_any()
            .downcast_ref::<TimestampMicrosecondArray>()
            .unwrap();
        assert_eq!(trade_ts.value(0), millis as i64 * 1_000);
        assert_eq!(trade_ts.timezone(), Some(ICEBERG_TZ));

        // Field ids survive on the arrow schema, which iceberg's parquet writer needs.
        let batch_schema = batch.schema();
        let field = batch_schema.field_with_name("trade_id").unwrap();
        assert_eq!(
            field.metadata().get("PARQUET:field_id").map(String::as_str),
            Some("4")
        );
    }

    #[test]
    fn partition_paths_use_iceberg_layout() {
        let schema = Arc::new(table_schema().unwrap());
        let spec = partition_spec(&schema).unwrap();
        // The day partition value is stored as epoch days but rendered as a date in the
        // directory name, i.e. `data/exchange=BINANCE/trade_ts_day=2026-08-07/...part`.
        let path = spec.partition_to_path(
            &iceberg::spec::Struct::from_iter([
                Some(iceberg::spec::Literal::string("BINANCE")),
                Some(iceberg::spec::Literal::date(20_672)),
            ]),
            schema.clone(),
        );
        assert_eq!(path, "exchange=BINANCE/trade_ts_day=2026-08-07");
    }
}
