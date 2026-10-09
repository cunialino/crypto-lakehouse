#!/usr/bin/env python3
"""Iceberg maintenance against Lakekeeper + Garage.

In-cluster this runs as the `table-maintenance` CronJob (02:00) — the flake bakes
this file into `custom.io/spark` at `/bin/table_maintenance.py`, and the image
entrypoint runs it with no arguments, so *the defaults below are the production
behaviour* and must stay what they were.

Locally, to compact one table without waiting for 02:00:

    kubectl -n lakekeeper port-forward svc/lakekeeper 8181:8181 &
    export LAKEKEEPER_URI=http://127.0.0.1:8181
    export GARAGE_ENDPOINT=http://192.168.0.200:3900          # garage-svc LB
    export AWS_ACCESS_KEY_ID=... AWS_SECRET_ACCESS_KEY=...     # same key pair
    export MAINT_TARGET_TABLE=lk.trades.trades_rust_test
    export MAINT_OPS=compact,manifests
    spark-submit --driver-memory 8g --conf spark.sql.autoBroadcastJoinThreshold=-1 \
        deploy/scripts/table_maintenance.py

Ops (`MAINT_OPS`, comma separated, run in this order; unknown names are an error
so a typo cannot silently do nothing):

    expire     expire_snapshots(older_than = now - MAINT_EXPIRE_HOURS, retain_last 1)
    compact    rewrite_data_files            <- the compaction
    manifests  rewrite_manifests
    orphans    remove_orphan_files           <- DELETES OBJECTS

Expiring thousands of snapshots needs a big driver: the reachable-file set is
broadcast, and a 2g driver dies with "Not enough memory to build and broadcast the
table to all worker nodes" (the CronJob runs `--driver-memory 4g`; locally I needed
`8g` plus `--conf spark.sql.autoBroadcastJoinThreshold=-1`). Note a failed expire may
already have committed part of its work — re-run to finish.

`expire` and `orphans` are destructive and should not run against a table a writer
is committing to: `orphans` relies on Iceberg's own grace window (days) to avoid
deleting objects the writer has flushed but not committed yet, and `expire` with
`retain_last => 1` also drops the history readers may still be pinning.
`compact` + `manifests` are safe alongside the batcher — worst case a commit
conflict makes the procedure fail, which is retryable.
"""

import os
import sys
from datetime import datetime, timedelta
from zoneinfo import ZoneInfo

from pyspark.sql import SparkSession

LAKEKEEPER_URI = os.getenv(
    "LAKEKEEPER_URI", "http://lakekeeper.lakekeeper.svc.cluster.local:8181"
)
GARAGE_ENDPOINT = os.getenv(
    "GARAGE_ENDPOINT", "http://garage-svc.garage.svc.cluster.local:3900"
)
GARAGE_REGION = os.getenv("GARAGE_REGION", "eu-lambronx-1")
TARGET_TABLE = os.getenv("MAINT_TARGET_TABLE", "lk.trades.trades")
OPS = [o.strip() for o in os.getenv("MAINT_OPS", "expire,compact,manifests,orphans").split(",") if o.strip()]
EXPIRE_HOURS = int(os.getenv("MAINT_EXPIRE_HOURS", "4"))
KNOWN_OPS = ("expire", "compact", "manifests", "orphans")

unknown = [o for o in OPS if o not in KNOWN_OPS]
if unknown:
    sys.exit(f"MAINT_OPS: unknown op(s) {unknown}, expected any of {KNOWN_OPS}")
if not TARGET_TABLE.startswith("lk."):
    sys.exit(f"MAINT_TARGET_TABLE: expected 'lk.<namespace>.<table>', got {TARGET_TABLE!r}")

spark = (
    SparkSession.builder.appName("LakekeeperGarageMaintenance")
    .config("spark.memory.fraction", "0.9")
    # The Rust writer (iceberg-rust) encodes BOOLEAN columns as plain RLE, which Spark's
    # vectorised Parquet reader refuses outright:
    #   UnsupportedOperationException: Cannot support vectorized reads for column
    #   [is_buyer_maker] required boolean is_buyer_maker = 8 with encoding RLE
    # so rewrite_data_files dies mid-write on any table the batcher wrote. This is not
    # specific to compaction: every Spark consumer of Rust-written files (this job, dbt)
    # needs vectorisation off, which is why it is set here rather than per invocation.
    .config("spark.sql.parquet.enableVectorizedReader", "false")
    .config("spark.sql.iceberg.vectorization.enabled", "false")
    # Event Logging
    .config("spark.eventLog.enabled", "true")
    .config("spark.eventLog.dir", "s3://lakehouse/spark-events/")
    .config("spark.eventLog.s3a.endpoint", GARAGE_ENDPOINT)
    .config("spark.eventLog.s3a.pathStyleAccess", "true")
    # Extensions & REST Catalog Setup
    .config(
        "spark.sql.extensions",
        "org.apache.iceberg.spark.extensions.IcebergSparkSessionExtensions",
    )
    .config("spark.sql.catalog.lk", "org.apache.iceberg.spark.SparkCatalog")
    .config("spark.sql.catalog.lk.type", "rest")
    .config("spark.sql.catalog.lk.uri", f"{LAKEKEEPER_URI}/catalog")
    .config("spark.sql.catalog.lk.warehouse", "crypto_lakehouse")
    # Iceberg Native FileIO Config (For metadata transactions)
    .config("spark.sql.catalog.lk.io-impl", "org.apache.iceberg.aws.s3.S3FileIO")
    .config("spark.sql.catalog.lk.s3.path-style-access", "true")
    # Hadoop FileSystem Map (Crucial fallback for Maintenance Procedures)
    .config("spark.hadoop.fs.s3a.impl", "org.apache.hadoop.fs.s3a.S3AFileSystem")
    .config("spark.hadoop.fs.s3.impl", "org.apache.hadoop.fs.s3a.S3AFileSystem")
    .config("spark.hadoop.fs.s3a.endpoint", GARAGE_ENDPOINT)
    .config("spark.hadoop.fs.s3a.path.style.access", "true")
    .config("spark.hadoop.fs.s3a.endpoint.region", GARAGE_REGION)
    .config("spark.hadoop.fs.s3a.access.key", os.getenv("AWS_ACCESS_KEY_ID", ""))
    .config("spark.hadoop.fs.s3a.secret.key", os.getenv("AWS_SECRET_ACCESS_KEY", ""))
    .config(
        "spark.hadoop.fs.s3a.aws.credentials.provider",
        "org.apache.hadoop.fs.s3a.SimpleAWSCredentialsProvider",
    )
    .getOrCreate()
)


def report(label: str) -> None:
    """Small before/after signal: compaction should cut files, never rows.

    The cache clear is not cosmetic: after a maintenance procedure the Spark session still
    holds the pre-procedure table metadata, so an `after` report without it reported the
    old snapshot count (434) while a fresh session already saw the expired one (7).
    """
    spark.catalog.clearCache()
    files = spark.sql(f"SELECT count(*) AS n, sum(file_size_in_bytes) AS b FROM {TARGET_TABLE}.files").collect()[0]
    snaps = spark.sql(f"SELECT count(*) AS n FROM {TARGET_TABLE}.snapshots").collect()[0]
    rows = spark.sql(f"SELECT count(*) AS n FROM {TARGET_TABLE}").collect()[0]["n"]
    print(f"[{label}] rows={rows} data_files={files['n']} bytes={files['b']} snapshots={snaps['n']}", flush=True)


print(f"table={TARGET_TABLE} ops={OPS} catalog={LAKEKEEPER_URI} s3={GARAGE_ENDPOINT}", flush=True)
report("before")

if "expire" in OPS:
    # The procedure only accepts a *literal* cutoff (`current_timestamp() - INTERVAL ...`
    # fails arg binding), and Spark reads a tz-naive literal in the session time zone. So
    # the cutoff must be formatted in that same zone: formatting UTC silently expired 2 h
    # less from a UTC+2 machine — harmless in the UTC CronJob pod, wrong the moment anyone
    # runs this locally. Ask Spark which zone it is instead of guessing.
    tz_name = spark.conf.get("spark.sql.session.timeZone", None)
    session_tz = ZoneInfo(tz_name) if tz_name else datetime.now().astimezone().tzinfo
    lookback = (datetime.now(session_tz) - timedelta(hours=EXPIRE_HOURS)).strftime(
        "%Y-%m-%d %H:%M:%S"
    )
    print(f"expiring snapshots older than {lookback} ({session_tz})", flush=True)
    spark.sql(f"""
        CALL lk.system.expire_snapshots(
            table => '{TARGET_TABLE}',
            older_than => TIMESTAMP '{lookback}',
            retain_last => 1
        )
    """)

if "compact" in OPS:
    print(f"Optimizing (compacting) data files for {TARGET_TABLE}...")
    spark.sql(
        f"""CALL lk.system.rewrite_data_files(
            table => '{TARGET_TABLE}',
            options => map(
                'max-concurrent-file-group-rewrites', '1',
                'min-input-files', '2'
            )
        )"""
    ).show()

if "manifests" in OPS:
    spark.sql(f"CALL lk.system.rewrite_manifests(table => '{TARGET_TABLE}')").show()

if "orphans" in OPS:
    print(f"Removing orphan files for {TARGET_TABLE}...")
    spark.sql(f"CALL lk.system.remove_orphan_files(table => '{TARGET_TABLE}', dry_run => false)").show()

report("after")
