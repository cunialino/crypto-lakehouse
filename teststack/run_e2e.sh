#!/usr/bin/env bash
# Local end-to-end harness for crypto-batcher: brings up the test stack (NATS +
# Lakekeeper + Postgres + S3), makes sure the bucket/warehouse/stream exist, and runs the
# test suite — unit + in-memory pipeline + live stack integration test.
#
#   ./teststack/run_e2e.sh              # up + all tests
#   ./teststack/run_e2e.sh --unit-only  # skip the live-stack test
#
# Nothing here touches the cluster: all endpoints are loopback, all data lives in podman
# volumes under the `lakehouse-it_*` names.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

UNIT_ONLY=0
[[ "${1:-}" == "--unit-only" ]] && UNIT_ONLY=1

WH_JSON="teststack/warehouse.json"
MINIO_CONTAINER=it-minio
NATS_PORT=14222
LK_PORT=18181
S3_PORT=19000
S3_BUCKET=lakehouse
E2E_WAREHOUSE=crypto_lakehouse_batcher
TABLE_NS=trades
TABLE=trades_batcher_it

# The warehouse file carries the local (throwaway) S3 credentials; keep them in vars so
# they are never echoed.
ACCESS_KEY=$(/usr/bin/env python3 -c "import json;print(json.load(open('$WH_JSON'))['storage-credential']['access-key-id'])")
SECRET_KEY=$(/usr/bin/env python3 -c "import json;print(json.load(open('$WH_JSON'))['storage-credential']['secret-access-key'])")

log() { printf '\n\033[1;36m==> %s\033[0m\n' "$*"; }

start_container() {
  local name=$1
  if [[ "$(podman ps -q -f name="^${name}$")" == "" ]]; then
    if [[ "$(podman ps -aq -f name="^${name}$")" == "" ]]; then
      echo "container $name does not exist - see teststack/README.md" >&2
      exit 1
    fi
    podman start "$name" >/dev/null
  fi
}

wait_for() {
  local url=$1 what=$2
  for _ in $(seq 1 60); do
    if curl -sf -m 2 -o /dev/null "$url"; then return 0; fi
    sleep 1
  done
  echo "timed out waiting for $what ($url)" >&2
  exit 1
}

log "starting containers"
start_container it-lakekeeper-db
start_container it-nats
start_container it-lakekeeper

# MinIO is the S3 endpoint the local lakekeeper warehouse expects on :19000.
if [[ "$(podman ps -q -f name="^${MINIO_CONTAINER}$")" == "" ]]; then
  if [[ "$(podman ps -aq -f name="^${MINIO_CONTAINER}$")" == "" ]]; then
    podman run -d --name "$MINIO_CONTAINER" --network host \
      -v lakehouse-it-minio:/data \
      -e MINIO_ROOT_USER="$ACCESS_KEY" -e MINIO_ROOT_PASSWORD="$SECRET_KEY" \
      quay.io/minio/minio:latest server /data \
      --address "127.0.0.1:${S3_PORT}" --console-address "127.0.0.1:19001" >/dev/null
  else
    podman start "$MINIO_CONTAINER" >/dev/null
  fi
fi

wait_for "http://127.0.0.1:${S3_PORT}/minio/health/live" "minio"
wait_for "http://127.0.0.1:${LK_PORT}/health" "lakekeeper"

log "ensuring bucket ${S3_BUCKET}"
curl -sf -m 10 -o /dev/null -X PUT \
  --aws-sigv4 "aws:amz:eu-lambronx-1:s3" --user "$ACCESS_KEY:$SECRET_KEY" \
  "http://127.0.0.1:${S3_PORT}/${S3_BUCKET}" ||
  curl -sf -m 10 -o /dev/null --aws-sigv4 "aws:amz:eu-lambronx-1:s3" \
    --user "$ACCESS_KEY:$SECRET_KEY" "http://127.0.0.1:${S3_PORT}/${S3_BUCKET}"

if [[ $UNIT_ONLY -eq 1 ]]; then
  log "cargo test (unit + in-memory pipeline)"
  cargo test -p crypto-batcher
  exit 0
fi

log "ensuring warehouse ${E2E_WAREHOUSE}"
if ! curl -sf -m 5 "http://127.0.0.1:${LK_PORT}/management/v1/warehouse" |
  grep -q "\"name\":\"${E2E_WAREHOUSE}\""; then
  /usr/bin/env python3 - "$WH_JSON" > /tmp/lk_batcher_warehouse.json <<'PY'
import json, sys
cred = json.load(open(sys.argv[1]))["storage-credential"]
print(json.dumps({
    "warehouse-name": "crypto_lakehouse_batcher",
    "project-id": "00000000-0000-0000-0000-000000000000",
    "storage-profile": {
        "type": "s3", "bucket": "lakehouse", "key-prefix": "batcher-lake",
        "assume-role-arn": None, "endpoint": "http://127.0.0.1:19000",
        "region": "eu-lambronx-1", "path-style-access": True,
        "flavor": "s3-compat", "sts-enabled": False, "push-s3-delete-disabled": True,
    },
    "storage-credential": {
        "type": "s3", "credential-type": "access-key",
        "access-key-id": cred["access-key-id"],
        "secret-access-key": cred["secret-access-key"],
    },
}))
PY
  curl -sf -m 20 -X POST -H 'Content-Type: application/json' \
    --data @/tmp/lk_batcher_warehouse.json \
    "http://127.0.0.1:${LK_PORT}/management/v1/warehouse" -o /dev/null
  rm -f /tmp/lk_batcher_warehouse.json
  echo "warehouse created"
else
  echo "warehouse already present"
fi

log "cargo test (unit + in-memory pipeline)"
cargo test -p crypto-batcher

log "live stack integration test (publish -> batch -> commit -> scan)"
NATS_URL="nats://127.0.0.1:${NATS_PORT}" \
LAKEKEEPER_URI="http://127.0.0.1:${LK_PORT}/catalog" \
LAKEKEEPER_WAREHOUSE="$E2E_WAREHOUSE" \
GARAGE_ENDPOINT="http://127.0.0.1:${S3_PORT}" \
GARAGE_REGION="eu-lambronx-1" \
GARAGE_ACCESS_KEY="$ACCESS_KEY" \
GARAGE_SECRET_KEY="$SECRET_KEY" \
ICEBERG_NAMESPACE="$TABLE_NS" \
ICEBERG_TABLE="$TABLE" \
BATCHER_CONSUMER="iceberg-batcher-e2e" \
BATCH_MAX_RECORDS=1000 \
BATCH_FLUSH_INTERVAL_SECS=5 \
E2E_ROWS="${E2E_ROWS:-3000}" \
  cargo test -p crypto-batcher --test live_stack -- --ignored --nocapture

log "reading the table back"
NATS_URL="nats://127.0.0.1:${NATS_PORT}" \
LAKEKEEPER_URI="http://127.0.0.1:${LK_PORT}/catalog" \
LAKEKEEPER_WAREHOUSE="$E2E_WAREHOUSE" \
GARAGE_ENDPOINT="http://127.0.0.1:${S3_PORT}" \
GARAGE_REGION="eu-lambronx-1" \
GARAGE_ACCESS_KEY="$ACCESS_KEY" \
GARAGE_SECRET_KEY="$SECRET_KEY" \
ICEBERG_NAMESPACE="$TABLE_NS" \
ICEBERG_TABLE="$TABLE" \
  cargo run -q -p crypto-batcher --example read_trades
