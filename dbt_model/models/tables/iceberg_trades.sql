{{ config(materialized='sink') }}

CREATE SINK IF NOT EXISTS iceberg_trades_sink
FROM {{ ref('nats_trades_mv') }}
WITH (
    connector = 'iceberg',
    type = 'upsert',
    primary_key = 'exchange,symbol,trade_id',
    enable_compaction = 'true',
    database.name = 'trades',
    create_table_if_not_exists = 'true',
    table.name = 'trades',
    catalog.type = 'rest',
    catalog.uri = 'http://lakekeeper.lakekeeper.svc.cluster.local:8181/catalog/',
    warehouse.path = 'crypto',
    s3.path.style.access = 'true',
    s3.endpoint = 'http://garage-svc.garage.svc.cluster.local:3900',
    s3.region = 'eu-lambronx-1',
    s3.access.key = '{{ env_var("GARAGE_ACCESS_KEY") }}',
    s3.secret.key = '{{ env_var("GARAGE_SECRET_KEY") }}',
    commit_checkpoint_interval = '900',
    commit_checkpoint_size_threshold_mb = '256',
    partition_by = 'identity(exchange),day(trade_ts)'
);
