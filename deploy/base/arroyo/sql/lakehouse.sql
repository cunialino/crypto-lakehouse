INSERT INTO iceberg_trades_sink
SELECT
  cast(event_time as bigint) AS event_time,
  symbol,
  exchange,
  cast(trade_id as bigint) AS trade_id,
  price,
  quantity,
  cast(trade_time as bigint) AS trade_time,
  is_buyer_maker,
  is_best_price_match,
  to_timestamp_millis(cast(trade_time as bigint)) AS trade_ts,
  to_timestamp_millis(cast(event_time as bigint)) AS event_ts
FROM nats_trades;
