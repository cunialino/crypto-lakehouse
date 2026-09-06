import os
from datetime import datetime, timedelta

from pyspark.sql import SparkSession

# Fetch environment variables for Garage and Lakekeeper
LAKEKEEPER_URI = "http://lakekeeper.lakekeeper.svc.cluster.local:8181"
GARAGE_ENDPOINT = "http://garage-svc.garage.svc.cluster.local:3900"
spark = (
    SparkSession.builder.appName("LakekeeperGarageMaintenance")
    .config("spark.memory.fraction", "0.9")
    # Event Logging
    .config("spark.eventLog.enabled", "true")
    .config("spark.eventLog.dir", f"s3://lakehouse/spark-events/")
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
    .config("spark.hadoop.fs.s3a.endpoint.region", "eu-lambronx-1")
.config("spark.hadoop.fs.s3a.access.key", os.getenv("AWS_ACCESS_KEY_ID", ""))
.config("spark.hadoop.fs.s3a.secret.key", os.getenv("AWS_SECRET_ACCESS_KEY", ""))
    .config(
        "spark.hadoop.fs.s3a.aws.credentials.provider",
        "org.apache.hadoop.fs.s3a.SimpleAWSCredentialsProvider",
    )
    .getOrCreate()
)

TARGET_TABLE = "lk.trades.trades"

lookback = (datetime.utcnow() - timedelta(hours=4)).strftime("%Y-%m-%d %H:%M:%S")


spark.sql(f"""
    CALL lk.system.expire_snapshots(
        table => '{TARGET_TABLE}',
        older_than => TIMESTAMP '{lookback}',
        retain_last => 1
    )
""")

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

spark.sql(
    f"""CALL lk.system.rewrite_manifests(
        table => '{TARGET_TABLE}'
    )"""
).show()


print(f"Removing orphan files for {TARGET_TABLE}...")
spark.sql(
    f"CALL lk.system.remove_orphan_files(table => '{TARGET_TABLE}', dry_run => false)"
).show()
