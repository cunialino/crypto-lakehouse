-- checkpoint_interval_micros: 60000000
INSERT INTO trade_gaps_sink
SELECT exchange, symbol, trade_id, next_trade_id - trade_id - 1 AS skipped
FROM (
  SELECT exchange, symbol, trade_id,
         lead(trade_id) over (partition by tumble(interval '1 minute') order by trade_id) AS next_trade_id
  FROM nats_trades
  WHERE cast(event_time as bigint) < cast(to_unixtime(now()) * 1000 - 60000 as bigint)
  GROUP BY tumble(interval '1 minute'), exchange, symbol, trade_id
)
WHERE next_trade_id IS NOT NULL
  AND next_trade_id - trade_id > 1
