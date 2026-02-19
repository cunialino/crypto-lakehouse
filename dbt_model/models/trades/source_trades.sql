{{ config(materialized='table_with_connector') }}

CREATE TABLE trade_events (
  *,
  trade_ts TIMESTAMP AS to_timestamp(trade_time / 1000.0),
  event_ts TIMESTAMP AS to_timestamp(event_time / 1000.0),
  PRIMARY KEY (exchange, symbol, trade_id)
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
)
FORMAT PLAIN ENCODE PROTOBUF (
    message='trade.data.TradeEventProto',
    schema.location='file:///etc/risingwave/schemas/trade_schema.pb'
);
