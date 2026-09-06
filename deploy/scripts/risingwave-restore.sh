#!/usr/bin/env bash
set -euo pipefail

SNAPSHOT_ID="${1:-}"
NS="${NS:-risingwave}"
PG_HOST="${PG_HOST:-pg-cluster-rw.cnpg-system.svc.cluster.local}"
PG_PORT="${PG_PORT:-5432}"
PG_USER="${PG_USER:-risingwave}"
PG_DB="${PG_DB:-risingwave}"
PG_ENDPOINT="$PG_HOST:$PG_PORT"
S3_ENDPOINT="${S3_ENDPOINT:-http://garage-svc.garage.svc.cluster.local:3900}"
S3_BUCKET="${S3_BUCKET:-hummock001}"
REGION="${REGION:-eu-lambronx-1}"
RW_IMAGE="${RW_IMAGE:-docker.risingwave.com/risingwavelabs/risingwave:v3.0.0}"

if [ -z "$SNAPSHOT_ID" ]; then
  echo "Usage: $0 <snapshot-id>" >&2
  echo "Example: $0 358" >&2
  exit 1
fi

if ! command -v kubectl &>/dev/null; then
  echo "ERROR: kubectl not found. Install it or run from a machine with kubectl access." >&2
  exit 1
fi

echo "=== RisingWave Restore to Snapshot $SNAPSHOT_ID ==="

echo "--- Fetching credentials ---"
PGPASS=$(kubectl get secret -n "$NS" rw-metastore-secret -o jsonpath={.data.password} | base64 -d)
S3_KEY=$(kubectl get secret -n "$NS" rustfs-secret -o jsonpath={.data.AWS_ACCESS_KEY_ID} | base64 -d)
S3_SECRET=$(kubectl get secret -n "$NS" rustfs-secret -o jsonpath={.data.AWS_SECRET_ACCESS_KEY} | base64 -d)

cleanup() {
  echo "--- Cleaning up temporary pods ---"
  kubectl delete pod -n "$NS" restore-pod --force --grace-period=0 2>/dev/null || true
  kubectl delete pod -n "$NS" pg-tool --force --grace-period=0 2>/dev/null || true
}
trap cleanup EXIT

echo "--- Scaling down all RW components ---"
kubectl scale statefulset -n "$NS" risingwave-meta --replicas=0
kubectl scale statefulset -n "$NS" risingwave-compute --replicas=0
kubectl scale deployment -n "$NS" risingwave-frontend --replicas=0
kubectl scale deployment -n "$NS" risingwave-compactor --replicas=0

echo "--- Creating temporary pods ---"
kubectl run -n "$NS" restore-pod \
  --image="$RW_IMAGE" \
  --restart=Never --command -- sleep 3600

kubectl run -n "$NS" pg-tool \
  --image=postgres:16 --restart=Never -- sleep 300

echo "--- Waiting for restore-pod ---"
kubectl wait --for=condition=Ready -n "$NS" pod/restore-pod --timeout=60s

echo "--- Waiting for pg-tool ---"
kubectl wait --for=condition=Ready -n "$NS" pod/pg-tool --timeout=60s

echo "--- Truncating all meta tables ---"
kubectl exec -n "$NS" pg-tool -- sh -c \
  "PGPASSWORD='$PGPASS' psql -h $PG_HOST -p $PG_PORT -U $PG_USER -d $PG_DB -c \"
TRUNCATE TABLE public.election_member, public.cluster, public.worker, public.object, public.election_leader, public.system_parameter, public.user_privilege, public.object_dependency, public.schema, public.actor, public.compaction_status, public.view, public.hummock_pinned_version, public.hummock_pinned_snapshot, public.hummock_version_delta, public.hummock_version_stats, public.hummock_sequence, public.catalog_version, public.compaction_task, public.compaction_config, public.session_parameter, public.subscription, public.secret, public.index, public.hummock_sstable_info, public.hummock_time_travel_version, public.hummock_time_travel_delta, public.hummock_epoch_to_version, public.hummock_gc_history, public.connection, public.function, public.exactly_once_iceberg_sink_metadata, public.fragment_relation, public.iceberg_tables, public.iceberg_namespace_properties, public.database, public.user_default_privilege, public.\"user\", public.cdc_table_snapshot_splits, public.fragment_splits, public.fragment, public.source, public.refresh_job, public.worker_property, public.\"table\", public.hummock_table_change_log, public.pending_sink_state, public.sink, public.streaming_job, public.seaql_migrations CASCADE;
\""

echo "--- Verifying tables are empty ---"
ROWS=$(kubectl exec -n "$NS" pg-tool -- sh -c \
  "PGPASSWORD='$PGPASS' psql -h $PG_HOST -p $PG_PORT -U $PG_USER -d $PG_DB -t -A -c \"
SELECT coalesce(sum(n_live_tup), 0) FROM pg_stat_user_tables;
\"" 2>&1 | tail -1)
echo "Remaining rows in meta tables: $ROWS"
if [ "$ROWS" != "0" ]; then
  echo "ERROR: Tables not empty after TRUNCATE ($ROWS rows remain). Re-run the script." >&2
  exit 1
fi

echo "--- Running restore-meta with snapshot $SNAPSHOT_ID ---"
kubectl exec -n "$NS" restore-pod -- env \
  AWS_ACCESS_KEY_ID="$S3_KEY" \
  AWS_SECRET_ACCESS_KEY="$S3_SECRET" \
  AWS_REGION="$REGION" \
  RW_S3_ENDPOINT="$S3_ENDPOINT" \
  RW_IS_FORCE_PATH_STYLE=true \
  ./risingwave/bin/risingwave ctl meta restore-meta \
  --meta-snapshot-id "$SNAPSHOT_ID" \
  --meta-store-type postgres \
  --sql-endpoint "$PG_ENDPOINT" \
  --sql-username "$PG_USER" \
  --sql-password "$PGPASS" \
  --sql-database "$PG_DB" \
  --hummock-storage-url "s3://$S3_BUCKET" \
  --hummock-storage-directory hummock \
  --backup-storage-url "s3://$S3_BUCKET" \
  --backup-storage-directory hummock/backup \
  --overwrite-hummock-storage-endpoint \
  --overwrite-backup-storage-url "s3://$S3_BUCKET" \
  --overwrite-backup-storage-directory hummock/backup

echo "--- Restore-meta succeeded! ---"

echo "--- Scaling up RW components ---"
kubectl scale statefulset -n "$NS" risingwave-meta --replicas=3
kubectl scale statefulset -n "$NS" risingwave-compute --replicas=2
kubectl scale deployment -n "$NS" risingwave-frontend --replicas=1
kubectl scale deployment -n "$NS" risingwave-compactor --replicas=1

echo "--- Waiting for meta-0 ---"
kubectl wait --for=condition=Ready -n "$NS" pod/risingwave-meta-0 --timeout=120s

echo "--- Waiting for compute-0 ---"
kubectl wait --for=condition=Ready -n "$NS" pod/risingwave-compute-0 --timeout=120s

echo "--- Waiting for frontend ---"
kubectl wait --for=condition=Ready -n "$NS" deployment/risingwave-frontend --timeout=120s

echo "--- Waiting for compactor ---"
kubectl wait --for=condition=Ready -n "$NS" deployment/risingwave-compactor --timeout=120s

echo "=== Restore complete ==="
