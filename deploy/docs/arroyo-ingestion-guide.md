# Arroyo ingestion guide (live setup walkthrough)

This documents how the running Arroyo 0.15.0 deployment actually ingests trades from NATS
`tradesstream` and writes them to Lakekeeper `trades.trades_2` (Garage S3 backed), plus the
trade-gap pipeline emitting to NATS core subject `trade.gaps`.

Everything below was validated against the live cluster (Aug 2026). The companion
`deploy/docs/arroyo-setup.md` covers chart/ArgoCD wiring; this file focuses on the four
things that were worked out by hand:

1. creating the Garage bucket and which keys access it,
2. creating the Arroyo pipelines,
3. giving Arroyo the protobuf schemas,
4. who creates the NATS (JetStream) consumer and how,
5. the two gotchas that made the Iceberg sink appear "dead" (no data files / no snapshots).

> Reference deck for this session lives in `deploy/base/arroyo/`:
> - `proto/trade_event.proto` — single source of truth for the schema (also compiled by
>   `rustapps/crypto-data`, whose `build.rs` reads it from here).
> - `sql/lakehouse.sql`, `sql/gaps.sql` — one SQL file per pipeline.
> - `register.sh` — the register Job entrypoint (validate + reconcile + create).
> - `kustomization.yaml` — `configMapGenerator` builds the `arroyo-pipelines` ConfigMap from
>   the above; `pipelines-register-job.yaml` is the idempotent ArgoCD Sync-hook register Job.
> The live session used the REST API directly (port-forward + `curl`); the register Job
> automates exactly the same calls.

## 0. Access the API (used throughout)

```sh
# keep the key in a var so it never lands in command output or shell history
API_KEY=$(kubectl -n arroyo get secret arroyo-api-key -o jsonpath='{.data.api-key}' \
  | base64 -d)

# background the port-forward (svc/arroyo port 80 -> local 8080)
setsid nohup kubectl -n arroyo port-forward svc/arroyo 8080:80 \
  </dev/null >/tmp/opencode/pf.log 2>&1 &

AUTH="Authorization: Bearer $API_KEY"
CT="Content-Type: application/json"
BASE="http://localhost:8080/api/v1"
```

If the API answers `401` without a bearer token and `200` with `$API_KEY`, the auth is working.

---

## 1. Garage bucket `arroyo001` and the keys that access it

This repo ships a `create_bucket.yaml`, but it is **not** used. The bucket was created once
with a plain S3 `PUT /arroyo001` against Garage's S3 endpoint using SigV4. The credentials
come from `garage-secret` in the `garage` namespace; those same credentials are mirrored into
`rustfs-secret` in the `arroyo` namespace so the controller/workers can use them.

### Keys (references only — actual values live in ArgoCD's bitwarden ExternalSecret)

| Use | Kubernetes secret | Key |
| --- | --- | --- |
| S3 access key id | ns `garage`, `garage-secret` | `secret_id` |
| S3 secret key | ns `garage`, `garage-secret` | `secret_key` |
| RPC secret (garage-internal, not used for S3) | ns `garage`, `garage-secret` | `rpc_secret` |
| Same access key id, mirrored for Arroyo | ns `arroyo`, `rustfs-secret` | `AWS_ACCESS_KEY_ID` |
| Same secret key, mirrored for Arroyo | ns `arroyo`, `rustfs-secret` | `AWS_SECRET_ACCESS_KEY` |
| S3 region | ns `arroyo`, controller env (literal) | `AWS_DEFAULT_REGION` = `eu-lambronx-1` |

Confirmed on the live cluster: `garage-secret.secret_id` decodes to the **same** value as
`rustfs-secret.AWS_ACCESS_KEY_ID` (`diff` is empty). `rustfs-secret` also carries `MINIO_USERNAME`/
`MINIO_PASSWORD` but those are not used by Arroyo.

### Create the bucket

Garage runs distroless (no shell), so the admin CLI can't be exec'd in. The S3 endpoint
(`garage-svc:3900`) accepts `PUT` bucket creation with SigV4:

```sh
AK=$(kubectl -n garage get secret garage-secret -o jsonpath='{.data.secret_id}' | base64 -d)
SK=$(kubectl -n garage get secret garage-secret -o jsonpath='{.data.secret_key}' | base64 -d)

setsid nohup kubectl -n garage port-forward svc/garage-svc 3900:3900 \
  </dev/null >/tmp/opencode/pf-garage.log 2>&1 &

curl -s -X PUT "http://127.0.0.1:3900/arroyo001" \
  --aws-sigv4 "aws:amz:eu-lambronx-1:s3" --user "$AK:$SK" -w "HTTP %{http_code}\n"
# HTTP 200
```

Verify:

```sh
curl -s -I "http://127.0.0.1:3900/arroyo001" \
  --aws-sigv4 "aws:amz:eu-lambronx-1:s3" --user "$AK:$SK" -o /dev/null -w "%{http_code}\n"
# 200
```

The controller references this bucket in `deploy/base/arroyo/` `arroyo-config` ConfigMap:
- checkpoints → `s3::http://garage-svc.garage.svc.cluster.local:3900/arroyo001/checkpoints`
- compiled artifacts → `s3::http://garage-svc.garage.svc.cluster.local:3900/arroyo001/artifacts`

New pipelines fail with `NoSuchBucket ... arroyo001` until this bucket exists, so create it
before any pipeline runs.

---

## 2. Creating the Arroyo pipelines

Pipelines are created via `POST /api/v1/pipelines`. **They auto-start on creation** — there is
no `/pipelines/{id}/start` route (`POST` there returns 404). A pipeline is:

```json
{
  "name": "arroyo-lakehouse",
  "query": "INSERT INTO iceberg_trades_sink ...",
  "udfs": [],
  "parallelism": 1,
  "checkpoint_interval_micros": 10000000
}
```

`checkpoint_interval_micros` is declarative, not hand-rolled: each `deploy/base/arroyo/sql/*.sql`
file carries a `-- checkpoint_interval_micros: <N>` header comment. `register.sh` reads it on
create and PATCHes it live on drift (no restart/replay).

Two are deployed (bodies at `/tmp/opencode/body_{lakehouse,gaps}.json` on the build host):

| Pipeline | Query writes | Sink table |
| --- | --- | --- |
| `arroyo-lakehouse` | `INSERT INTO iceberg_trades_sink SELECT ... FROM nats_trades` | `iceberg_trades_sink` (Iceberg/parquet → lakekeeper `trades.trades_2`) |
| `arroyo-trade-gaps` | `INSERT INTO trade_gaps_sink SELECT ... FROM (SELECT tumble(...) ...) GROUP BY ...` | `trade_gaps_sink` (NATS core subject `trade.gaps`, JSON) |

### SQL rules that were discovered live (arroyo 0.15 specifics)

- **`cast(x as int64)` fails**: "Unsupported SQL type Int64" comes from
  `crates/arroyo-planner/src/types.rs` (it handles `BigInt`, not `Int64`). Use
  `cast(x as bigint)` and, for the Iceberg table registration, `type:"int64"`.
- **`tumble(interval '1 minute')` returns a `Struct{start,end}`**, not a timestamp. Dot access
  on the expr fails. Compute the window in an inner query and select `window.start` from it
  (pattern proven by `crates/arroyo-sql-testing/src/test/queries/session_window.sql`).
- **`uint64` columns** fail with "Unsupported Arrow data type: UInt64" in the Iceberg sink, so
  the source schema field is registered `uint64` in the proto but the SQL casts to `bigint`.
- JSON sent raw to the API must have **all keys quoted** (unquoted keys only work when jq is
  involved; the server 400s with `key must be a string`).

### The two `arroyo` pipeline SQL bodies

```sql
-- arroyo-lakehouse
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
  -- IMPORTANT: trade_time/event_time are epoch MILLISECONDS, so it must be
  -- to_timestamp_millis. Using to_timestamp_micros (expects microseconds)
  -- silently puts every row at the 1970-01-21 partition.
  to_timestamp_millis(cast(trade_time as bigint)) AS trade_ts,   -- bigint, not uint64
  to_timestamp_millis(cast(event_time as bigint)) AS event_ts
FROM nats_trades;
```

```sql
-- arroyo-trade-gaps
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
  AND next_trade_id - trade_id > 1;
```

### Create / validate

```sh
# validate before creating (returns {"graph":...} or {"errors":[...]})
curl -s -H "$AUTH" -H "$CT" -X POST "$BASE/pipelines/validate_query" \
  --data-binary @/tmp/opencode/body_lakehouse.json

# create (auto-starts)
curl -s -o /dev/null -w "%{http_code}\n" -H "$AUTH" -H "$CT" -X POST "$BASE/pipelines" \
  --data-binary @/tmp/opencode/body_lakehouse.json
# 200

curl -s -H "$AUTH" "$BASE/jobs" | jq -c '.data[] | {id, state, running_desired}'
```

### Restart / delete dance (used repeatedly while debugging)

Stopping/deleting requires the job to reach a terminal state first:

```sh
# stop force, then delete
curl -s -H "$AUTH" -H "$CT" -X PATCH "$BASE/pipelines/$PID" -d '{"stop":"force"}'
sleep 3
curl -s -o /dev/null -w "%{http_code}\n" -H "$AUTH" -X DELETE "$BASE/pipelines/$PID"
```

If the SQL or a referenced connection table changes, you must **delete the pipelines first**,
because `connection_tables` with a live consumer are locked against deletion
(`"Cannot delete connection_table; it is still being used by pipelines"`).

---

## 3. Giving Arroyo the protobuf schemas

Arroyo does not read `.proto` files at runtime. The schema is embedded **in the connection
table** (`nats_trades`, the NATS JetStream source) as an inline protobuf descriptor string.

Source of truth: `deploy/base/arroyo/proto/trade_event.proto` (single source; the collector
`rustapps/crypto-data` `build.rs` compiles the same file, so there is no second copy to drift):

```proto
syntax = "proto3";
package trade.data;
message TradeEventProto {
  uint64 event_time = 1;
  string symbol = 2;
  string exchange = 3;
  uint64 trade_id = 4;
  double price = 5;
  double quantity = 6;
  uint64 trade_time = 7;
  bool is_buyer_maker = 8;
  bool is_best_price_match = 9;
}
```

The `nats_trades` table is registered with this shape (schema.definition carries the proto
source; `fields` is empty so Arroyo infers field types from the descriptor):

```json
{
  "name": "nats_trades",
  "connector": "nats",
  "connection_profile_id": "<nats_cluster profile id>",
  "config": { "connectorType": { "sourceType": { "stream": "tradesstream", "rateLimit": 0 } } },
  "schema": {
    "format": { "type": "protobuf", "message_name": "trade.data.TradeEventProto" },
    "bad_data": { "behavior": "drop" },
    "fields": [],
    "definition": { "type": "protobuf_schema", "schema": "<full proto text>", "dependencies": {} },
    "inferred": false,
    "primary_keys": []
  }
}
```

`register.sh` (mounted at `/pipelines/register.sh` from the generated ConfigMap) does exactly
this: it jq-encodes the mounted proto (`/pipelines/trade_event.proto`) into
`schema.definition.schema`. The **Iceberg sink** table (`iceberg_trades_sink`) is registered
separately with explicit `fields` (int64/timestamps matching the SQL output) and
`"definition": { "type": "json_schema", "schema": "" }` — not a protobuf definition.

Timestamps are registered with **singular** serde names: `unit:"microsecond"` (not
`microseconds`) — `crates/arroyo-rpc/src/api_types/connections.rs` uses `TimestampUnit`
serde names `second|millisecond|microsecond|nanosecond`.

---

## 4. The NATS (JetStream) consumer: who creates it and how

No `Consumer` CR is deployed (the `consumers.jetstream.nats.io` CRD exists and
jetstream-controller 0.23.0 is running, but a `Consumer` CR would be a separate, unused
consumer — Arroyo never adopts pre-created consumers).

**The consumer is created by the Arroyo worker at job start**, not by us. See
`crates/arroyo-connectors/src/nats/source/mod.rs`:

1. Consumer name is deterministic per job: `format!("{}-{}-{}", stream, operator_id_without_prefix, job_id_without_prefix)`.
   `job_id` is a random nanoid (`generate_id(IdTypes::JobConfig)` → `job_<10 chars>`), so each
   job spawns its own consumer.
2. On start the operator does `delete_consumer(name)` (ignore errors) then `create_consumer(config)`.
3. It then calls `consumer.info()` to read the ack floor.

Observed consumers in the cluster all follow this naming, e.g.
`tradesstream-source_nats_trades_0-GRpzRwYETg`, plus the pre-existing `risingwave_consumer`
(unrelated, created by RisingWave).

### Two gotchas that had to be fixed for nats-server 2.14.2

- **`rateLimit` must be 0.** Arroyo defaults it to `-1` and async-nats 0.38.0 serializes it as
  a `u64`, producing `"rate_limit": 18446744073709551615`, which nats-server 2.14.2 rejects
  (`json: unknown field "rate_limit"` — that field only exists in 2.15+). Setting the source
  table config option to `0` (`{"connectorType":{"sourceType":{...,"rateLimit":0}}}`) makes
  async-nats skip the field entirely (its `skip_serializing_if = "is_default"`).
- **Run the pipeline at `parallelism: 1`.** With parallelism > 1 every source subtask shares
  the *same* consumer name and races on `delete`/`create`/`info`. A subtask calling `info()`
  right after another subtask deletes the consumer gets a 404 error JSON without `stream_name`,
  and arroyo's `.expect("Failed to get info from the NATS consumer.")` panics ("missing field
  `stream_name`"). At parallelism 1 the single subtask owns the consumer, so the race is gone.

### Check the consumer from nats-box

```sh
kubectl -n nats exec deploy/nats-cluster-box -- nats con ls -s nats://nats-cluster:4222 tradesstream
kubectl -n nats exec deploy/nats-cluster-box -- nats con info -s nats://nats-cluster:4222 \
  tradesstream <consumer-name> -j
```

---

## 5. The Iceberg sink: why no data files / no snapshots (two gotchas)

Symptoms seen during bring-up: the NATS source consumed the whole stream (checkpoints advancing,
6.1M messages, 641MB), the `[filesystem_sink]` node ran with no errors, yet `trades_2` never got a
`data/` directory, never got a snapshot, and lakekeeper `table_snapshot` stayed empty.

Both root causes are in `crates/arroyo-connectors/src/filesystem/`:

### 5a. No rolling policy = files never roll = never commit

The sink only calls Iceberg `commit()` when it has `finished_files` (files that were rolled /
closed). The set of rolling policies is `RollingPolicy::from_file_settings` in
`sink/mod.rs`, and the only policy that is always present is the hard limit
`RollingPolicy::PartLimit(1000 parts)`:

```rust
// sink/mod.rs — from_file_settings
policies.push(RollingPolicy::PartLimit(1000));          // always present
if let Some(d) = table.rolling_policy.file_size_bytes   { policies.push(SizeLimit(d)) }
if let Some(s) = table.rolling_policy.inactivity_seconds{ policies.push(InactivityDuration(s)) }
if let Some(s) = table.rolling_policy.interval_seconds  { policies.push(RolloverDuration(s)) }
```

`RollingPolicy` (struct in `config.rs`) has three config keys, all optional:
`rolling_policy.file_size` (bytes), `rolling_policy.inactivity_interval`, `rolling_policy.interval`.
With **none of them set**, the only trigger is `PartLimit(1000 × DEFAULT_TARGET_PART_SIZE)`
(DEFAULT_TARGET_PART_SIZE = 32MiB → ~32GiB). A sink can run for hours, buffer megabytes into its
checkpoint state (`arroyo001/.../operator-sink_*/table-r-000`, an embedded parquet), and hold an
open multipart upload against the table's `data/` path, without ever producing a single commit.

Fix — set a rolling policy on the **connection table** (there is no update endpoint for
connection tables in `rest.rs`, only create/delete, so delete + recreate the sink table, then
re-register the pipeline):

```json
"rolling_policy": { "interval_seconds": 900 }   // roll every 15 min
```

The `IcebergSink` config is `serde(rename_all="snake_case", deny_unknown_fields)`, so the JSON
keys are `rolling_policy.interval_seconds` / `inactivity_seconds` / `file_size_bytes` — and since
`deny_unknown_fields` is set, don't send keys the struct doesn't know. `IcebergSink` (`config.rs`)
also requires `type: "sink"`, `namespace`, `table_name`, and may carry `partitioning` and
`storage_options` as before.

Symptoms of an un-configured rolling policy to watch for:

- `S3 ?uploads` shows an open multipart upload at `<table>/data/*.parquet` that never closes.
- The sink's checkpoint object `table-r-000` (parquet) grows while `table-p-000` stays ~331B.
- `table.metadata().current_snapshot()` stays `null` and `metadata/` holds only the initial
  `00000-*.gz.metadata.json`.

### 5b. The table must declare partition fields explicitly (partition-expr validation)

At pipeline-graph build time arroyo validates the connect table's `partitioning.fields` against
the *registered* Iceberg schema (`IcebergPartitioning::partition_expr` in
`crates/arroyo-planner/src/tables.rs`). If you register the sink table with `partitioning.fields`
referencing `exchange`/`trade_ts` but the table's `fields` are empty, the build 500s with
`partition field 'identity(exchange)' does not exist in the schema (Valid fields: ["_timestamp"])`.

Fix: the sink connection table must declare explicit `fields` (int64/string/float64 + two
`timestamp` `unit:"microsecond"` columns matching the SQL output) — see section 3.

### Health check / verification

```sh
# source has a live consumer and advancing acks
kubectl -n nats exec deploy/nats-cluster-box -- nats con ls -s nats://nats-cluster:4222 tradesstream

# sink checkpoint state (pending writers ≈ tiny, rolled data grows)
AK=$(kubectl -n garage get secret garage-secret -o jsonpath='{.data.secret_id}' | base64 -d)
SK=$(kubectl -n garage get secret garage-secret -o jsonpath='{.data.secret_key}' | base64 -d)
K="hummock001/lake/019e3049-1844-7e73-845a-35d91b881e37/019fe7fd-65d6-7123-b786-f4edec2e2f4e"
curl -s "http://127.0.0.1:3900/$K" --aws-sigv4 "aws:amz:eu-lambronx-1:s3" --user "$AK:$SK" | tr '<' '\n' | grep -oE 'Key>[^<]*' | sed 's/Key>//'  # expect data/ + metadata/ after first roll

# lakekeeper: tabular → table UUIDs, and whether trades_2 has snapshots
kubectl -n cnpg-system exec pg-cluster-1 -- psql -U postgres -d lakekeeper -tAc \
  "SELECT tabular_id, tabular_namespace_name, name FROM public.tabular ORDER BY name;"
```

Table UUIDs currently in lakekeeper (namespace `{trades}`):

| UUID | Table | Writer |
| --- | --- | --- |
| `019eded3-cbb9-7091-b96e-3e490b2c3265` | `trades` | RisingWave (still running), separated `_p_*` partition dirs |
| `019fe7fd-65d6-7123-b786-f4edec2e2f4e` | `trades_2` | Arroyo `arroyo-lakehouse` (`iceberg_trades_sink`) |
| `019faf61-94d6-7191-b000-f47aacb863a0` | `wholes` | older/other workload (commits stop weeks before this session) |
| `019f6cc6-47c9-7840-9993-e8c5039b4cd8` | *(orphan, unregistered)* | stale table left over from earlier sink runs — has leftover parquet, no lakekeeper entry |

---

## Notes / non-goals

- The active pipelines write to `trades.trades_2` (Iceberg) to avoid colliding with the still
  running RisingWave deployment that owns `trades.trades`. Spark maintenance (`query.py` /
  `tmp.py`) targets `trades.trades` only, so `trades_2` is isolated. The sink auto-creates the
  table on first commit (verified: lakekeeper `tabular` shows `trades_2`, with a single
  metadata object under `hummock001/lake/<namespace>/<table>/metadata/`).
- `trades_2` had to be dropped and repopulated once: the pipeline originally used
  `to_timestamp_micros()` on the source's epoch-**millis** `trade_time`/`event_time`, silently
  parking every row at the `1970-01-21` partition. The live pipeline now uses
  `to_timestamp_millis()`. If you ever inspect `trades_2` and see a `1970-01-21` partition, the
  timestamp conversion is wrong again. This is also a useful sanity check for any future
  Iceberg sink that emits timestamps.
- Note iceberg-rs 0.6.0 miscounts the snapshot summary: `commit()` writes the manifest first and
  only then builds the summary from an already-drained `added_data_files` vec, so snapshots report
  `total-records: 0` and `total-data-files: 0` even when they hold millions of rows. The manifest
  avros under `metadata/snap-*.avro` are authoritative; the summary is cosmetic.
- The `lakehouse.sql` gap-checking semantics (dedup, schema drift) are intentionally not covered
  here — this guide documents what was done to get the pipelines running end to end. The file
  sink's rolling-policy interval **is** covered (section 5): `iceberg_trades_sink` is registered
  with `rolling_policy.interval_seconds: 900` so a parquet file rolls and commits every 15
  minutes.
- Repo wiring is now in sync with the live run after this session: `deploy/base/arroyo/sql/*.sql`
  are byte-identical to the two live pipeline queries (`to_timestamp_millis`, inner-outer
  `gaps.sql` shape with `window.start` + `cast(x as bigint)`); `proto/trade_event.proto` is the
  single schema source; `register.sh` registers `rateLimit: 0`, the explicit sink `fields`,
  `rolling_policy.interval_seconds: 900`, `connection_profile_id`, and `parallelism: 1`.
  Registration is gitops-driven: `kustomization.yaml` `configMapGenerator` builds the
  `arroyo-pipelines` ConfigMap, and the `pipelines-register-job.yaml` Sync hook runs it.
- **How SQL/schema changes deploy (single push):** the register Job is idempotent on every
  sync. When git SQL/proto drifts from what is registered, the drifted pipeline is
  stop(force) -> delete -> recreated (full NATS JetStream replay; dupes until Spark dedup).
  Broken artifacts never reach that step — `validate_query` / proto `schemas/test` abort the
  sync first. So a change to `sql/*.sql` or `proto/trade_event.proto` deploys with one push.