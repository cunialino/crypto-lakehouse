# Local test stack (podman, loopback only)

Used to exercise `crypto-batcher` end to end without touching the cluster:
**NATS JetStream → Lakekeeper REST catalog → S3 → Iceberg scan**.

```
it-nats          nats 2.14.6      JetStream on :14222   (stream: tradesstream, exchange.*)
it-lakekeeper-db postgres 17     :15433
it-lakekeeper    lakekeeper v0.12.2  REST catalog on :18181  (/catalog, /management)
it-minio         minio            S3 on :19000, console :19000  (bucket: lakehouse)
```

All containers use podman's `host` network; everything is reachable on `127.0.0.1`.

## Run everything

```sh
./teststack/run_e2e.sh                 # start stack + unit + in-memory + live-stack tests
./teststack/run_e2e.sh --unit-only     # no containers/live test needed
E2E_ROWS=20000 ./teststack/run_e2e.sh  # bigger live run
```

The script is idempotent: it starts stopped containers, creates the `lakehouse` bucket,
creates the `crypto_lakehouse_batcher` warehouse in Lakekeeper if missing, then runs
`cargo test -p crypto-batcher` and `cargo test -p crypto-batcher --test live_stack --
--ignored`, and finally prints the table back with the `read_trades` example.

## Manual driving

```sh
podman start it-lakekeeper-db it-lakekeeper it-nats it-minio

# publish synthetic trades
NATS_URL=nats://127.0.0.1:14222 cargo run -p crypto-batcher --example publish_trades -- 5000

# run the batcher (creds come from teststack/warehouse.json, never printed)
NATS_URL=nats://127.0.0.1:14222 \
LAKEKEEPER_URI=http://127.0.0.1:18181/catalog \
LAKEKEEPER_WAREHOUSE=crypto_lakehouse_batcher \
GARAGE_ENDPOINT=http://127.0.0.1:19000 GARAGE_REGION=eu-lambronx-1 \
GARAGE_ACCESS_KEY=... GARAGE_SECRET_KEY=... \
ICEBERG_NAMESPACE=trades ICEBERG_TABLE=trades_batcher_it \
BATCH_MAX_RECORDS=2000 BATCH_FLUSH_INTERVAL_SECS=5 \
  cargo run -q -p crypto-batcher

# read it back
cargo run -q -p crypto-batcher --example read_trades   # same LAKEKEEPER_*/GARAGE_* env

# consumer state (= pipeline offset)
podman run --rm --network host natsio/nats-box:0.14.0 \
  nats -s nats://127.0.0.1:14222 con info tradesstream iceberg-batcher-it
```

## Files

| Path | What |
| --- | --- |
| `warehouse.json` | the Lakekeeper warehouse definition used to bootstrap this stack; it also carries the **local-only** S3 credentials that `run_e2e.sh` reads (never echoed) |
| `garage.toml` | single-node Garage config (see note below); the `it-garage` container bind-mounts it at `/etc/garage.toml` |
| `run_e2e.sh` | the harness described above |
| `.secrets/` | leftover secrets from an earlier session — gitignored, and the S3 key in there no longer exists in Garage |

## Note on Garage vs MinIO

`it-garage` exists but its `teststack/garage.toml` was missing (so it could not start) and
its volumes contained **no buckets and no keys**. The Lakekeeper warehouse registered in
this stack points at `http://127.0.0.1:19000` with the static credentials from
`warehouse.json`, which is a MinIO topology, so:

* `teststack/garage.toml` was recreated (config only — its secrets come from the
  `GARAGE_RPC_SECRET` / `GARAGE_ADMIN_TOKEN` env vars already set on the container), and
* `it-garage` is currently **stopped** (not deleted; its volumes are untouched) while
  `it-minio` serves :19000.

To go back to Garage: `podman stop it-minio && podman start it-garage`, then create a
bucket + key (`podman exec it-garage /garage bucket create lakehouse`,
`/garage key create …`, `/garage bucket allow --read --write lakehouse --key …`) and
register a warehouse with those credentials via
`POST http://127.0.0.1:18181/management/v1/warehouse`.

## Warehouses in the local Lakekeeper

| Name | Storage | Used by |
| --- | --- | --- |
| `crypto_lakehouse` | `s3://lakehouse/lake` | pre-existing (from an earlier session; its stored S3 key no longer exists, so it currently fails storage validation) |
| `crypto_lakehouse_batcher` | `s3://lakehouse/batcher-lake` | created by `run_e2e.sh`, used by the batcher tests |
