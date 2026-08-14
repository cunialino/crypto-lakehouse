-- checkpoint_interval_micros: 60000000
INSERT INTO trade_gaps_sink
SELECT exchange, symbol, window.start AS window_start,
       min_trade_id, max_trade_id, events, skipped
FROM (
  SELECT tumble(interval '1 minute') AS window,
         exchange, symbol,
         cast(min(trade_id) as bigint) AS min_trade_id,
         cast(max(trade_id) as bigint) AS max_trade_id,
         count(*) AS events,
         cast(max(trade_id) as bigint) - cast(min(trade_id) as bigint) + 1 - count(*) AS skipped
  FROM nats_trades
  GROUP BY window, exchange, symbol
)
