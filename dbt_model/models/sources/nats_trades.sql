{{ config(materialized='source') }}
CREATE SOURCE IF NOT EXISTS {{ this }} (
  *,
  trade_ts TIMESTAMP AS to_timestamp(trade_time / 1000.0),
  event_ts TIMESTAMP AS to_timestamp(event_time / 1000.0),
  WATERMARK FOR trade_ts AS trade_ts - INTERVAL '5 seconds'
)
WITH (
  connector = 'nats',
  server_url = 'nats://nats-cluster.nats.svc.cluster.local:4222',
  connect_mode = 'plain',
  subject = 'exchange.*',
  stream = 'tradesstream',
  consumer.durable_name = 'risingwave_consumer',
  scan.startup.mode = 'earliest',
  consumer.ack_policy   = 'explicit',
  consumer.max_ack_pending = 10000,

)
FORMAT PLAIN ENCODE PROTOBUF (
    message='trade.data.TradeEventProto',
    schema.location='file:///etc/risingwave/schemas/trade_schema.pb'
);
