{{ config(materialized='materialized_view') }}

SELECT DISTINCT ON (exchange, symbol, trade_id) *
FROM {{ ref('nats_trades') }}
WHERE trade_ts > CURRENT_TIMESTAMP - INTERVAL '4 days'
