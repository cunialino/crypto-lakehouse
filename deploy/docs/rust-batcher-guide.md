# crypto-batcher: NATS → Iceberg micro-batching in Rust (replaces the Arroyo pipeline)

`rustapps/crypto-batcher` is a single binary that does what the Arroyo `arroyo-lakehouse`
pipeline did — read trade events off NATS JetStream and write them into the Lakekeeper-
managed Iceberg table — without a stream processor, a checkpoint store, or a compiled
artifact pipeline.

```text
NATS JetStream tradesstream (exchange.*)
  └─ durable PULL consumer, explicit ack              ← the pipeline offset lives HERE
       └─ decode trade.data.TradeEventProto (prost)
            └─ Accumulator: max_records | max_bytes | flush_interval   ← the micro-batch
                 └─ RecordBatch → dedup anti-join → RecordBatchPartitionSplitter (identity exchange, day trade_ts)
                      └─ FanoutWriter(DataFileWriter(RollingFileWriter(ParquetWriter zstd)))
                           └─ Transaction::fast_append().commit()       ← one snapshot per batch
                                └─ ack every message in the batch       ← at-least-once
```

Module map (`rustapps/crypto-batcher/src/`):

| File | Responsibility |
| --- | --- |
| `config.rs` | env → `NatsConfig` / `IcebergConfig` / `BatchConfig` |
| `schema.rs` | the 11-column `trades` Iceberg schema, `identity(exchange), day(trade_ts)` spec, rows → `RecordBatch` |
| `source.rs` | JetStream durable pull consumer, protobuf decode, `Ack` handle, consumer-config reconciliation |
| `batcher.rs` | the accumulator (flush triggers, dedup) and the run loop (`flush` → commit → ack) |
| `sink.rs` | REST catalog + S3 (opendal), table auto-create, dedup anti-join, parquet writing, retried `fast_append` commit |
| `main.rs` | CLI, tracing, SIGTERM/SIGINT drain |

## Why this shape

* **The durable consumer is the offset.** Nothing is acknowledged until the Iceberg commit
  succeeds, so the only state the pipeline owns is the consumer's ack floor. A crash
  loses no data and needs no checkpoint replay: un-acked messages come back after
  `ack_wait`. This is the property that lets the whole Arroyo job/state disappear.
* **One micro-batch = one snapshot.** Commits are atomic and visible immediately; there is
  no "files opened but never rolled" failure mode (see the Arroyo guide §5a).
* **Flush policy is size- AND age-bounded**, whichever of `BATCH_MAX_RECORDS`,
  `BATCH_MAX_BYTES`, `BATCH_FLUSH_INTERVAL_SECS` fires first, so a quiet stream still
  commits within `flush_interval` and a busy one stays under the memory bound.
* **Parquet files roll at `ICEBERG_TARGET_FILE_SIZE`**, so a giant micro-batch becomes
  several files in the *same* snapshot instead of one huge object.

## Delivery semantics

* **At-least-once.** Rows are written before they are acknowledged; a crash between commit
  and ack redelivers and rewrites them (visible as duplicate `(exchange, symbol,
  trade_id)` rows, exactly like Arroyo's checkpoint-replay duplicates).
* **Dedup on `(exchange, symbol, trade_id)`.** Repeats inside one micro-batch are dropped
  and still acked; `BATCH_DEDUP=false` turns that off. Before writing, the batch is also
  anti-joined against the rows already committed in the `(exchange, trade_ts)` ranges it
  touches, which is what makes a redelivered micro-batch a no-op rather than a duplicate.
  The key matches the Spark maintenance job's `concat(exchange, trade_id, symbol)`, and
  that job (`deploy/scripts/table_maintenance.py`) stays responsible for a full-table
  dedup: the probe deliberately never reads outside those ranges.
* **Bad payloads are dropped and acked** (`bad_data: drop` equivalent), so a poison pill
  cannot stall the pipeline.
* **Commit failures are retried** `COMMIT_RETRIES` times with exponential backoff and a
  metadata reload between attempts (concurrent committers just cost a retry). If they
  still fail, the batch is *not* acked and the loop keeps running; parquet written by the
  failed attempt is orphaned and reclaimed by table maintenance.

## Configuration (all env)

| Env | Default | Meaning |
| --- | --- | --- |
| `NATS_URL` | — (required) | `nats://host:4222` |
| `TRADES_STREAM` | `tradesstream` | JetStream stream to read |
| `TRADES_SUBJECT_FILTER` | `exchange.*` | consumer subject filter |
| `BATCHER_CONSUMER` | `iceberg-batcher` | durable consumer name = pipeline offset |
| `BATCHER_FETCH_BATCH` | `2000` | messages per pull request |
| `BATCHER_FETCH_EXPIRES_SECS` | `2` | pull long-poll |
| `BATCHER_ACK_WAIT_SECS` | `300` | redelivery timer; auto-raised to `4 × flush_interval + 10 s` if too small |
| `LAKEKEEPER_URI` | in-cluster lakekeeper | REST catalog uri |
| `LAKEKEEPER_WAREHOUSE` | `crypto_lakehouse` | warehouse name |
| `LAKEKEEPER_CREDENTIAL` | unset | `client_id:secret` for catalog OAuth2 |
| `ICEBERG_NAMESPACE` / `ICEBERG_TABLE` | `trades` / `trades_rust` | destination table (created if missing) |
| `GARAGE_ENDPOINT` / `GARAGE_REGION` | in-cluster garage / `eu-lambronx-1` | S3 endpoint + region |
| `GARAGE_ACCESS_KEY` / `GARAGE_SECRET_KEY` | — (required) | S3 key, never logged |
| `GARAGE_PATH_STYLE` | `true` | path-style addressing |
| `ICEBERG_TARGET_FILE_SIZE` | `128MiB` | parquet roll size (accepts `128MiB`, `2GB`, raw bytes) |
| `ICEBERG_PARQUET_COMPRESSION` | `zstd` | `zstd`, `snappy`, `gzip`, `lz4_raw`, `none` |
| `COMMIT_RETRIES` | `5` | metadata commit attempts |
| `BATCH_MAX_RECORDS` | `50000` | row trigger |
| `BATCH_MAX_BYTES` | `32MiB` | payload trigger |
| `BATCH_FLUSH_INTERVAL_SECS` | `30` | age trigger |
| `BATCH_DEDUP` | `true` | intra-batch dedup |

`--ensure-only` connects, creates namespace/table if absent, prints the location and
exits — used as the k8s init container so a bad catalog/S3 config shows up immediately
instead of after the first batch.

## Table contract

Identical to the retired `iceberg_trades_sink` connection table, so downstream readers do
not care which writer produced the files:

```
event_time long, symbol string, exchange string, trade_id long, price double,
quantity double, trade_time long, is_buyer_maker bool, is_best_price_match bool,
trade_ts timestamptz(micros), event_ts timestamptz(micros)
partitioned by: identity(exchange), day(trade_ts)   → data/exchange=X/trade_ts_day=YYYY-MM-DD/
```

Field ids 1…11 are fixed in `schema.rs` — never renumber them.
`trade_ts`/`event_ts` are **milliseconds × 1000** (the Arroyo guide's `1970-01-21`
partition trap): if you ever see a `trade_ts_day=1970-01-21` partition, that conversion
broke again.

## Deploy

```
deploy/apps/crypto-lakehouse-batcher.yaml   ArgoCD Application → ns crypto-batcher
deploy/base/batcher/kustomization.yaml
deploy/base/batcher/batcher.yaml            Deployment (replicas: 1, strategy: Recreate)
deploy/base/batcher/batcher_config.yaml     all knobs above
deploy/base/batcher/garage_s3_secret.yaml   ExternalSecret → GARAGE_ACCESS_KEY/SECRET_KEY
```

Image: `nix build .#dockerBatcher` (pushed as `192.168.0.2:5000/crypto-batcher` by the
`ARC Crypto Builder` workflow on `v*` tags, next to `crypto-collector`).

Notes:

* `replicas: 1` + `Recreate` on purpose: the offset is a single durable consumer, so two
  live replicas only fight over `max_ack_pending`. To scale, shard (e.g. one consumer +
  one table per exchange) rather than adding replicas.
* `terminationGracePeriodSeconds: 120` gives SIGTERM time to flush, commit and ack.
* The sink creates the namespace/table on first run with the schema and partition spec
  above; nothing else has to be pre-created.

## Cutover from Arroyo (not executed)

Nothing was removed from the live Arroyo deployment. To cut over:

1. Deploy the batcher (it writes `trades.trades_rust` by default; set `ICEBERG_TABLE` to
   whatever should replace `trades_2`).
2. Let it drain `tradesstream` (72 h retention) — `nats con info -s … tradesstream
   iceberg-batcher` shows `Unprocessed Messages: 0` when caught up.
3. Compare against Arroyo's table with the Spark dedup/count queries
   (`deploy/scripts/table_maintenance.py`, or `crypto-iceberg-query`).
4. Only then stop the Arroyo pipelines and drop their tables; the `trade_gaps` pipeline is
   **not** ported (see gaps below), so keep Arroyo for that or port the tumble/lead query.

## Local validation

`./teststack/run_e2e.sh` runs unit tests, the in-memory catalog pipeline tests, and the
live-stack test (real NATS + Lakekeeper + S3, all on loopback). See `teststack/README.md`.

Validated locally on 2026-10-07:

* 20 unit + 4 in-memory pipeline tests green (`cargo test -p crypto-batcher`).
* 5 000 events → 3 micro-batches (2 count-triggered, 1 interval-triggered), 8 parquet
  files, 4 partitions, `total-records: 5000`, read back 5 000 rows through the catalog.
* Crash test: SIGKILL with a buffered-but-uncommitted batch left the table unchanged;
  after `ack_wait` JetStream redelivered and the restarted process committed it
  (7 000 → 8 000 rows, no loss).
* `live_stack_end_to_end` (ignored by default): 3 000 published → 3 000 readable, no
  duplicate trade ids.

## Known gaps vs the Arroyo deployment

* **No `trade_gaps` pipeline** (`deploy/base/arroyo/sql/gaps.sql`, NATS `trade.gaps`) —
  that one is a tumbling-window `lead()` aggregation, not a batch sink.
* **No Prometheus metrics/Alertmanager rules** (Arroyo had `prometheus_rules.yaml` +
  `prometheus_podmonitor.yaml`). The batcher logs one structured line per flush
  (`rows`, `files`, `partitions`, `bytes`, `write_ms`, `commit_ms`, `duplicates_dropped`,
  `ack_failures`, running totals) plus `consumer ready` with `num_pending` /
  `num_ack_pending`; a `/metrics` endpoint would be a small follow-up.
* **Snapshot expiry / compaction** is not done by the batcher; the existing Spark
  maintenance job is still the place where `expire_snapshots`/rewrite happens.
* Single-process throughput: one pull consumer, one writer. Locally it committed
  ~2 000 rows/batch in ~100 ms write + ~70 ms commit, which is far beyond the Binance
  stream rate, but it is one pod.
