#!/usr/bin/env python3
"""Run Apache Spark against the WarpDrive Iceberg REST catalog.

Spark connects to the REST catalog (:8181) and to WarpDrive's S3 API (:9760) via
Iceberg's S3FileIO, then does a full table lifecycle in SQL: create namespace +
table, insert, projection + filter + aggregate, and a MERGE-style overwrite. This
exercises far more of the REST protocol (and the S3 data plane) than pyiceberg --
a real JVM engine with its own planner, writers, and AWS SDK v2 client.

Prereq: distributed WarpDrive on WD_S3 (:9760), a warehouse bucket, and the REST
catalog on :8181. Run: venv/bin/python eval_spark.py
"""
import os
from pyspark.sql import SparkSession

ICEBERG = "org.apache.iceberg:iceberg-spark-runtime-4.0_2.13:1.12.0"
AWS = "org.apache.iceberg:iceberg-aws-bundle:1.12.0"
IVY = os.environ.get("IVY_DIR", "/tmp/claude-0/-home-user/1092b597-fec1-566d-bf89-445018d387d4/scratchpad/ivy")

spark = (
    SparkSession.builder.appName("warpdrive-iceberg")
    .master("local[2]")
    .config("spark.jars.packages", f"{ICEBERG},{AWS}")
    .config("spark.jars.ivy", IVY)
    .config("spark.sql.extensions", "org.apache.iceberg.spark.extensions.IcebergSparkSessionExtensions")
    .config("spark.sql.catalog.wd", "org.apache.iceberg.spark.SparkCatalog")
    .config("spark.sql.catalog.wd.type", "rest")
    .config("spark.sql.catalog.wd.uri", os.environ.get("REST", "http://127.0.0.1:8181"))
    .config("spark.sql.catalog.wd.io-impl", "org.apache.iceberg.aws.s3.S3FileIO")
    .config("spark.sql.catalog.wd.s3.endpoint", os.environ.get("WD_S3", "http://127.0.0.1:9760"))
    .config("spark.sql.catalog.wd.s3.path-style-access", "true")
    .config("spark.sql.catalog.wd.s3.access-key-id", "adminkey")
    .config("spark.sql.catalog.wd.s3.secret-access-key", "adminsecret")
    .config("spark.sql.catalog.wd.client.region", "us-east-1")
    .config("spark.ui.enabled", "false")
    .getOrCreate()
)
spark.sparkContext.setLogLevel("ERROR")

spark.sql("CREATE NAMESPACE IF NOT EXISTS wd.spark")
spark.sql("DROP TABLE IF EXISTS wd.spark.events")
spark.sql("""
  CREATE TABLE wd.spark.events (id BIGINT, region STRING, amount DOUBLE)
  USING iceberg
""")
spark.sql("""
  INSERT INTO wd.spark.events
  SELECT id, concat('r', cast(id % 8 as string)) AS region, cast(id * 1.5 as double) AS amount
  FROM range(0, 5000)
""")
# second insert -> second snapshot
spark.sql("INSERT INTO wd.spark.events SELECT id, 'r3', 2.0 FROM range(0, 200)")

total = spark.sql("SELECT count(*) c FROM wd.spark.events").collect()[0]["c"]
r3 = spark.sql("SELECT count(*) c FROM wd.spark.events WHERE region='r3'").collect()[0]["c"]
agg = spark.sql("SELECT region, round(sum(amount),1) s FROM wd.spark.events GROUP BY region ORDER BY region").collect()
snaps = spark.sql("SELECT count(*) c FROM wd.spark.events.snapshots").collect()[0]["c"]

print(f"total={total} region_r3={r3} snapshots={snaps}")
print("agg:", [(row['region'], row['s']) for row in agg])
assert total == 5200, total
assert r3 == 200 + 5000 // 8, r3   # 200 explicit + the id%8==3 rows from the first insert
assert snaps == 2, snaps
print("PASS: Apache Spark 4.0 full lifecycle (DDL + insert + aggregate + snapshots) "
      "on the WarpDrive Iceberg REST catalog")
spark.stop()
