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

### 2. Create a temporary pod with the risingwave binary

```bash
kubectl run -n risingwave restore-pod \
  --image=docker.risingwave.com/risingwavelabs/risingwave:v3.0.0 \
  --restart=Never -- sleep 3600
```

### 3. Drop the meta store database and recreate it (empty)

Grant CREATEDB to the risingwave user if needed:

```bash
kubectl patch cluster -n cnpg-system pg-cluster --type='json' \
  -p='[{"op": "replace", "path": "/spec/managed/roles/0", "value": {
    "comment": "role used for risingwave", "connectionLimit": -1,
    "createdb": true, "ensure": "present", "inherit": true,
    "login": true, "name": "risingwave",
    "passwordSecret": {"name": "rw-metastore-secret"},
    "superuser": false
  }}]'
```

Wait a few seconds, then extract the PG password and drop/recreate:

```bash
export RW_PG_PASS=$(kubectl -n risingwave get secret rw-metastore-secret \
  -o jsonpath={.data.password} | base64 -d)

kubectl run -n risingwave pg-drop --image=postgres:16 --restart=Never -- sleep 30

kubectl exec -n risingwave pg-drop -- bash -c "
PGPASSWORD=$RW_PG_PASS psql -h pg-cluster-rw.cnpg-system.svc.cluster.local \
  -U risingwave -d postgres -c 'DROP DATABASE IF EXISTS risingwave;'
PGPASSWORD=$RW_PG_PASS psql -h pg-cluster-rw.cnpg-system.svc.cluster.local \
  -U risingwave -d postgres -c 'CREATE DATABASE risingwave;'
"
```

### 4. Initialize the database schema

Start the meta-node briefly so it runs all migrations and creates the tables:

```bash
export S3_KEY=$(kubectl -n risingwave get secret rustfs-secret \
  -o jsonpath={.data.AWS_ACCESS_KEY_ID} | base64 -d)
export S3_SECRET=$(kubectl -n risingwave get secret rustfs-secret \
  -o jsonpath={.data.AWS_SECRET_ACCESS_KEY} | base64 -d)

kubectl run -n risingwave meta-init \
  --image=docker.risingwave.com/risingwavelabs/risingwave:v3.0.0 \
  --restart=Never --command -- bash -c "
export AWS_ACCESS_KEY_ID=$S3_KEY
export AWS_SECRET_ACCESS_KEY=$S3_SECRET
export AWS_REGION=eu-lambronx-1
export RW_S3_ENDPOINT=http://garage-svc.garage.svc.cluster.local:3900
export RW_IS_FORCE_PATH_STYLE=true
export RW_STATE_STORE=hummock+s3://hummock001
export RW_DATA_DIRECTORY=hummock
export RW_LISTEN_ADDR=0.0.0.0:5690
export RW_BACKEND=postgres
export RW_SQL_ENDPOINT=pg-cluster-rw.cnpg-system.svc.cluster.local:5432
export RW_SQL_USERNAME=risingwave
export RW_SQL_PASSWORD=$RW_PG_PASS
export RW_SQL_DATABASE=risingwave
timeout 25 ./risingwave/bin/risingwave meta-node || true
"
```

The meta-node will panic on cluster_id conflict but the schema will be fully initialized.

### 5. Truncate all data tables (keep empty schema)

```bash
kubectl exec -n risingwave pg-drop -- bash -c "
PGPASSWORD=$RW_PG_PASS psql -h pg-cluster-rw.cnpg-system.svc.cluster.local \
  -U risingwave -d risingwave -c \"
SELECT 'TRUNCATE TABLE ' || string_agg(quote_ident(schemaname) || '.' || quote_ident(tablename), ', ') || ' CASCADE;'
FROM pg_tables WHERE schemaname = 'public';
\" 2>&1 | grep TRUNCATE | tail -1
"
```

Run the generated TRUNCATE statement, then add:

```sql
TRUNCATE TABLE public.seaql_migrations;
```

### 6. Run restore-meta

```bash
kubectl exec -n risingwave restore-pod -- env \
  AWS_ACCESS_KEY_ID=$S3_KEY \
  AWS_SECRET_ACCESS_KEY=$S3_SECRET \
  AWS_REGION=eu-lambronx-1 \
  RW_S3_ENDPOINT=http://garage-svc.garage.svc.cluster.local:3900 \
  RW_IS_FORCE_PATH_STYLE=true \
  ./risingwave/bin/risingwave ctl meta restore-meta \
  --meta-snapshot-id <ID> \
  --meta-store-type postgres \
  --sql-endpoint pg-cluster-rw.cnpg-system.svc.cluster.local:5432 \
  --sql-username risingwave \
  --sql-password "$RW_PG_PASS" \
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

### 7. Scale up RisingWave components

```bash
kubectl scale statefulset -n risingwave risingwave-meta --replicas=3
kubectl scale statefulset -n risingwave risingwave-compute --replicas=2
kubectl scale deployment -n risingwave risingwave-frontend --replicas=1
kubectl scale deployment -n risingwave risingwave-compactor --replicas=1
```

### 8. Verify

- NATS consumer `risingwave_consumer` is recreated automatically on the `tradesstream`
- Check: `nats con info tradesstream risingwave_consumer`
- Pipeline starts processing from earliest available NATS messages
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
