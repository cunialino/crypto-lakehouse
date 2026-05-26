{{ config(materialized='materialized_view') }}

SELECT *
FROM {{ ref('nats_trades') }}
WHERE trade_ts > CURRENT_TIMESTAMP - INTERVAL '2 days'
