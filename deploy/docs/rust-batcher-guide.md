# crypto-batcher: NATS → Iceberg micro-batching in Rust

`rustapps/crypto-batcher` is the single writer of `trades.trades`: it reads trade events off
NATS JetStream and writes them into the Lakekeeper-managed Iceberg table without a stream
processor, a checkpoint store, or a compiled artifact pipeline.

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
  no "files opened but never rolled" failure mode.
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
| `ICEBERG_NAMESPACE` / `ICEBERG_TABLE` | `trades` / `trades` | destination table (created if missing) |
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
`trade_ts`/`event_ts` are **milliseconds × 1000**: treat millisecond input as microseconds
and every row lands in `trade_ts_day=1970-01-21`, which is how this pipeline failed once
before.

The pre-cutover `trades` table carried the same eleven columns but every one *nullable* and
`trade_ts`/`event_ts` typed `timestamp` **without** timezone, partitioned by fields named
`exchange_identity`/`trade_ts_day`. Arrow refuses that mix (`Timestamp(µs)` vs
`Timestamp(µs, "+00:00")`) and Iceberg cannot convert the type in place, which is why the
cutover moved tables aside instead of writing into the old one.

## Deploy

```
deploy/apps/crypto-lakehouse-batcher.yaml   ArgoCD Application → ns crypto-batcher
deploy/base/batcher/kustomization.yaml
deploy/base/batcher/batcher.yaml            Deployment (replicas: 1, strategy: Recreate)
deploy/base/batcher/batcher_config.yaml     all knobs above
deploy/base/batcher/garage_s3_secret.yaml   ExternalSecret → GARAGE_ACCESS_KEY/SECRET_KEY
```

Image: `nix build .#dockerBatcher` (pushed as `192.168.0.2:5000/crypto-batcher` by the
`ARC Crypto Builder` workflow on `v*` tags, next to `crypto-collector`). The deployment uses
`:latest` with `imagePullPolicy: Always`, so a rollout re-pulls whatever the registry holds;
pin a tag in `batcher.yaml` when you want a reproducible one.

Notes:

* `replicas: 1` + `Recreate` on purpose: the offset is a single durable consumer, so two
  live replicas only fight over `max_ack_pending`. To scale, shard (e.g. one consumer +
  one table per exchange) rather than adding replicas.
* `terminationGracePeriodSeconds: 120` gives SIGTERM time to flush, commit and ack.
* The sink creates the namespace/table on first run with the schema and partition spec
  above; nothing else has to be pre-created.

## Cutover (2026-11-03)

The Arroyo and RisingWave deployments are deleted (ArgoCD applications `arroyo` and
`risingwave`, together with `deploy/base/arroyo`, `deploy/base/risingwave` and the dbt job
that ran in the RisingWave namespace). What replaced them:

1. Lakekeeper renames — metadata only, no objects copied: `trades.trades` →
   `trades.trades_legacy`, then `trades.trades_rust_test` → `trades.trades`. The prod name
   therefore keeps the rows the batcher had already written *and* stays the name every reader
   defaults to (`MAINT_TARGET_TABLE=lk.trades.trades`, `crypto-dedup --table trades.trades`).
2. `deploy/base/batcher` deployed with `ICEBERG_TABLE=trades` and a **fresh** durable
   consumer `iceberg-batcher`, so it replays `tradesstream`'s 72 h retention; the cross-batch
   dedup anti-join turns the already-present part of that replay into a no-op.
3. `crypto-backfill --from trades.trades_legacy --into trades.trades <start> <end> --execute`
   copies the history the batcher never saw; run 2026-11-03 in weekly windows over
   2026-06-24 → 2026-10-07. It anti-joins on `(exchange, symbol, trade_id)`, so a re-run or an
   overlapping window cannot duplicate rows, and it normalises the legacy nullable columns on
   the way in.
   The append is deliberately not a SQL `INSERT`: `iceberg-datafusion` 0.10 resolves a
   partition's source columns *by position*, while DataFusion hoists repeated projection
   arguments into leading `__common_expr_N` columns, so `Day::transform` was handed a `Utf8`
   column (`Should not call internally for unsupported data type Utf8`) — loud for
   `day(trade_ts)`, silent and wrong for `identity(exchange)` whenever both sources are
   strings. `--execute` instead streams the candidates through `crypto_iceberg_query::append`
   (the batcher's own writer) with each chunk bound to the destination schema, so a mismatch is
   a refused write instead of a mis-partitioned file. Every chunk is one `fast_append` snapshot,
   and the run only calls itself good after reading the window back through the destination's
   partitions — rows filed under a wrong `day` are invisible to a pruned read.
   Result: every legacy key up to 2026-10-17 is in `trades.trades` (`rows_to_append: 0` for
   each window audited afterwards), one row per key. That makes some days *smaller* than
   `trades_legacy`, which is the point: the old table double-wrote whole days — 2026-09-05 and
   2026-09-06 hold exactly twice their distinct keys — and the copy kept one row each.
4. `trades_legacy` is dropped later the same day: `DELETE …/tables/trades_legacy?purging=true`
   against the REST catalog (HTTP 204; it held ~5.5 GB), after re-auditing the start, middle and
   end of its range and getting `rows_to_append: 0` each time.
   Two things to know before dropping a table here. This Lakekeeper keeps no soft-delete
   bookkeeping — the `tabular` row is simply gone, there is no undrop — so the coverage audit is
   the only safety net. And with no garbage collector deployed, whether `purging=true` actually
   reclaimed the objects shows only in Garage's bucket usage, which is not observable from the
   catalog side; assume the ~5.5 GB may still be sitting there until checked.
   Nothing unique left with it. What it held beyond `trades.trades` was ~68 M redundant copies
   (518 603 480 rows over ~450 M distinct keys): the double-written days, which the copy
   deliberately did not bring across.
   One incidental fact worth keeping: its files sat under `data/exchange_identity=…/…` while the
   batcher writes `data/exchange=…/…`. Different directory names are harmless — readers use the
   partition *spec*, never the paths — which is also what made the rename in step 1 safe.

### Clearing the batcher's own fossils (open)

`trades.trades` still holds 19 984 duplicate rows: 2026-10-07 (653), 2026-10-12 (2 845) and
2026-10-13 (16 486), all of them inside one hour each. They predate the pre-commit anti-join,
and they are exact copies — the distinct *rows* of the affected window equal its distinct keys —
so a replay, not conflicting data. Nothing new accumulates. Clearing them is a rewrite rather
than an update, so it ends in a name swap:

```text
# 1. destination with the same schema and partition spec (no NATS needed)
ICEBERG_TABLE=trades_repaired crypto-batcher --ensure-only

# 2. copy, one row per (exchange, symbol, trade_id), day by day; the source is untouched
crypto-dedup --table trades.trades --insert-into trades.trades_repaired <start> <end>

# 3. scale the batcher down, dedup the tail, rename `trades` -> `trades_with_dupes` and
#    `trades_repaired` -> `trades`, scale back up. Same consumer, so the replay of
#    `tradesstream` is absorbed by the anti-join exactly as in the cutover above.
```

Verified on 2026-11-03 against a scratch target: the 2026-10-13 19:00 hour (147 595 rows,
16 486 redundant) rewrote to 131 109 rows, re-audited at 0 duplicates, readable through pruned
filters on both `exchange` and `trade_ts`.

### Storage & catalog facts

* Bucket `lakehouse` in Garage. One S3 key is shared by the Lakekeeper storage profile,
  `garage-s3-credentials` (batcher), `lakekeeper-secret` (Spark maintenance) and
  `garage-secret` (ns `garage`).
* The storage profile has **remote signing** enabled and STS disabled: Lakekeeper signs S3
  requests on the client's behalf, so the key a client presents is a bootstrap rather than
  the authority. From off-cluster: port-forward `svc/lakekeeper` (8181) and point
  `GARAGE_ENDPOINT` at the Garage LB (`192.168.0.200:3900`), which is what
  `deploy/scripts/table_maintenance.py` documents.
* **Never read a row count out of a snapshot summary.** iceberg-rust writes the manifest and
  only then builds the summary from an already-drained list, so `total-records` /
  `total-data-files` can read `0` on a table holding hundreds of millions of rows, and
  `expire_snapshots` + `rewrite_manifests` make it worse. Trust the manifest avros,
  `crypto-dedup`/`crypto-backfill`'s audit output, or a real scan (Spark logs
  `ScanReport{resultDataFiles, totalFileSizeInBytes}`).
* Lakekeeper mirrors metadata into its own `lakekeeper` DB (`tabular`, `table_snapshot_log`,
  `table_default_partition_spec`), which is the quick way to ask "does this table exist, when
  did it last commit, what is its schema". `expire … retain_last 1` trims
  `table_snapshot_log` to the surviving snapshot, so it cannot say how much data is in the
  table.

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

## Known gaps

* **No Prometheus metrics/Alertmanager rules.** The batcher logs one structured line per flush
  (`rows`, `files`, `partitions`, `bytes`, `write_ms`, `commit_ms`, `duplicates_dropped`,
  `ack_failures`, running totals) plus `consumer ready` with `num_pending` /
  `num_ack_pending`; a `/metrics` endpoint would be a small follow-up.
* **Snapshot expiry / compaction** is not done by the batcher; the existing Spark
  maintenance job is still the place where `expire_snapshots`/rewrite happens.
* Single-process throughput: one pull consumer, one writer. Locally it committed
  ~2 000 rows/batch in ~100 ms write + ~70 ms commit, which is far beyond the Binance
  stream rate, but it is one pod.
