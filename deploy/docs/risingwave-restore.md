# RisingWave: Restore from Meta Snapshot

## Prerequisites

- RisingWave 3.0.0 (`docker.risingwave.com/risingwavelabs/risingwave:v3.0.0`)
- Meta store: PostgreSQL (host `pg-cluster-rw.cnpg-system.svc.cluster.local:5432`, database `risingwave`)
- State store: S3-compatible (Garage, bucket `hummock001`)
- S3 credentials in secret `rustfs-secret` in namespace `risingwave`
- PG credentials in secret `rw-metastore-secret` in namespace `risingwave`

## Restore Steps

### 1. List available snapshots

```bash
kubectl exec -n nats <nats-box-pod> -- sh -c '
AWS_ACCESS_KEY_ID=$(kubectl -n risingwave get secret rustfs-secret -o jsonpath={.data.AWS_ACCESS_KEY_ID} | base64 -d) \
AWS_SECRET_ACCESS_KEY=$(kubectl -n risingwave get secret rustfs-secret -o jsonpath={.data.AWS_SECRET_ACCESS_KEY} | base64 -d) \
mc alias set garage http://garage-svc.garage.svc.cluster.local:3900 \
  "$AWS_ACCESS_KEY_ID" "$AWS_SECRET_ACCESS_KEY" --api s3v4 2>/dev/null
mc ls garage/hummock001/hummock/backup/ | grep ".snapshot"
'
```

The backup manifest is at `hummock/backup/manifest.json`.

### 2. Scale down all RW components

```bash
kubectl scale statefulset -n risingwave risingwave-meta --replicas=0
kubectl scale statefulset -n risingwave risingwave-compute --replicas=0
kubectl scale deployment -n risingwave risingwave-frontend --replicas=0
kubectl scale deployment -n risingwave risingwave-compactor --replicas=0
```

### 3. Create a restore pod and a pg-tool pod

```bash
kubectl run -n risingwave restore-pod \
  --image=docker.risingwave.com/risingwavelabs/risingwave:v3.0.0 \
  --restart=Never --command -- sleep 3600

kubectl run -n risingwave pg-tool \
  --image=postgres:16 --restart=Never -- sleep 120
```

### 4. Get the PG password

```bash
PGPASS=$(kubectl get secret -n risingwave rw-metastore-secret \
  -o jsonpath={.data.password} | base64 -d)
```

### 5. Generate and run the TRUNCATE statement

Generate the truncate statement:

```bash
kubectl run -n risingwave gen-truncate --image=postgres:16 --restart=Never -- sleep 30
kubectl exec -n risingwave gen-truncate -- sh -c "PGPASSWORD='$PGPASS' psql -h pg-cluster-rw.cnpg-system.svc.cluster.local -U risingwave -d risingwave -c \"
SELECT 'TRUNCATE TABLE ' || string_agg(quote_ident(schemaname) || '.' || quote_ident(tablename), ', ') || ' CASCADE;'
FROM pg_tables WHERE schemaname = 'public';
\"" 2>&1 | grep TRUNCATE | tail -1
```

Create a SQL file with the generated statement and copy it to the pod:

```bash
cat > /tmp/truncate.sql << 'SQLEOF'
<generated TRUNCATE statement from above>;
SQLEOF
kubectl cp /tmp/truncate.sql -n risingwave gen-truncate:/tmp/truncate.sql
kubectl exec -n risingwave gen-truncate -- sh -c "PGPASSWORD='$PGPASS' psql -h pg-cluster-rw.cnpg-system.svc.cluster.local -U risingwave -d risingwave -f /tmp/truncate.sql"
kubectl delete pod -n risingwave gen-truncate --force --grace-period=0
rm -f /tmp/truncate.sql
```

Alternatively, if you're sure the schema already exists (it does for v3.0.0), this single command works:

```bash
kubectl exec -n risingwave pg-tool -- sh -c "PGPASSWORD='$PGPASS' psql -h pg-cluster-rw.cnpg-system.svc.cluster.local -U risingwave -d risingwave -c \"
TRUNCATE TABLE public.election_member, public.cluster, public.worker, public.object, public.election_leader, public.system_parameter, public.user_privilege, public.object_dependency, public.schema, public.actor, public.compaction_status, public.view, public.hummock_pinned_version, public.hummock_pinned_snapshot, public.hummock_version_delta, public.hummock_version_stats, public.hummock_sequence, public.catalog_version, public.compaction_task, public.compaction_config, public.session_parameter, public.subscription, public.secret, public.index, public.hummock_sstable_info, public.hummock_time_travel_version, public.hummock_time_travel_delta, public.hummock_epoch_to_version, public.hummock_gc_history, public.connection, public.function, public.exactly_once_iceberg_sink_metadata, public.fragment_relation, public.iceberg_tables, public.iceberg_namespace_properties, public.database, public.user_default_privilege, public.\"user\", public.cdc_table_snapshot_splits, public.fragment_splits, public.fragment, public.source, public.refresh_job, public.worker_property, public.\"table\", public.hummock_table_change_log, public.pending_sink_state, public.sink, public.streaming_job, public.seaql_migrations CASCADE;
\"" 2>&1
```

### 6. Get S3 credentials

```bash
S3_KEY=$(kubectl get secret -n risingwave rustfs-secret \
  -o jsonpath={.data.AWS_ACCESS_KEY_ID} | base64 -d)
S3_SECRET=$(kubectl get secret -n risingwave rustfs-secret \
  -o jsonpath={.data.AWS_SECRET_ACCESS_KEY} | base64 -d)
```

### 7. Run restore-meta

```bash
kubectl exec -n risingwave restore-pod -- env \
  AWS_ACCESS_KEY_ID="$S3_KEY" \
  AWS_SECRET_ACCESS_KEY="$S3_SECRET" \
  AWS_REGION=eu-lambronx-1 \
  RW_S3_ENDPOINT=http://garage-svc.garage.svc.cluster.local:3900 \
  RW_IS_FORCE_PATH_STYLE=true \
  ./risingwave/bin/risingwave ctl meta restore-meta \
  --meta-snapshot-id <ID> \
  --meta-store-type postgres \
  --sql-endpoint pg-cluster-rw.cnpg-system.svc.cluster.local:5432 \
  --sql-username risingwave \
  --sql-password "$PGPASS" \
  --sql-database risingwave \
  --hummock-storage-url s3://hummock001 \
  --hummock-storage-directory hummock \
  --backup-storage-url s3://hummock001 \
  --backup-storage-directory hummock/backup \
  --overwrite-hummock-storage-endpoint \
  --overwrite-backup-storage-url s3://hummock001 \
  --overwrite-backup-storage-directory hummock/backup
```

Expect output: `command succeeded` (takes ~1 second).

### 8. Scale up RisingWave components

```bash
kubectl scale statefulset -n risingwave risingwave-meta --replicas=3
kubectl scale statefulset -n risingwave risingwave-compute --replicas=2
kubectl scale deployment -n risingwave risingwave-frontend --replicas=1
kubectl scale deployment -n risingwave risingwave-compactor --replicas=1
```

### 9. Verify

- NATS consumer `risingwave_consumer` reconnects automatically on the `tradesstream`
- Check: `nats con info tradesstream risingwave_consumer`
- Pipeline resumes processing from the consumer's position in the stream
- Iceberg sink deduplicates by `primary_key = exchange,symbol,trade_id`

## Key Details

| Setting | Value |
|---|---|
| S3 endpoint | `http://garage-svc.garage.svc.cluster.local:3900` |
| Region | `eu-lambronx-1` |
| Bucket | `hummock001` |
| Hummock directory | `hummock` |
| Backup directory | `hummock/backup` |
| Force path style | `RW_IS_FORCE_PATH_STYLE=true` |
| Restore-meta validation | Disabled (`--validate-integrity` omitted) — older SSTs {7,20,15,46} were legitimately GC'd |
| PG host | `pg-cluster-rw.cnpg-system.svc.cluster.local:5432` |
| PG credentials | secret `rw-metastore-secret` in namespace `risingwave` |
| S3 credentials | secret `rustfs-secret` in namespace `risingwave` |

## Notes

- The database schema is **not** dropped — tables already exist from the same version. Only data is truncated.
- `restore-meta` checks that every meta table has zero rows before restoring. The TRUNCATE satisfies this.
- The `overwrite-*` flags ensure the restored meta state uses the current storage config (S3 endpoint, path style, etc.) rather than the outdated config baked into the snapshot.
