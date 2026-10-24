#!/usr/bin/env python3
"""Wall-clock A/B in Spark: a 1-column aggregate on bench.plain vs bench.placed,
median of N runs, with the server's shard-bytes-read delta per query.

Honest caveat baked into the output: on a single loopback cluster with a warm
cache, fewer bytes read may NOT translate to less wall-clock (decode + Spark
overhead dominate); the bytes-read column is the deployment-independent truth and
the wall-clock column shows what this rig actually yields. Run bench_build.py
first. Run: SPARK_LOCAL_IP=127.0.0.1 venv/bin/python bench_spark_time.py
"""
import os, time, json, statistics, urllib.request
from pyspark.sql import SparkSession

WD_S3 = os.environ.get("WD_S3", "http://127.0.0.1:9760")
REST = os.environ.get("REST", "http://127.0.0.1:8181")
IVY = "/tmp/claude-0/-home-user/1092b597-fec1-566d-bf89-445018d387d4/scratchpad/ivy"
RUNS = int(os.environ.get("RUNS", "7"))
WARMUP = 2

def read_bytes():
    with urllib.request.urlopen(f"{WD_S3}/cluster/_internal/read_stats", timeout=5) as r:
        return json.load(r)["shard_bytes_read"]

spark = (
    SparkSession.builder.appName("warpdrive-bench").master("local[2]")
    .config("spark.jars.packages", "org.apache.iceberg:iceberg-spark-runtime-4.0_2.13:1.12.0,org.apache.iceberg:iceberg-aws-bundle:1.12.0")
    .config("spark.jars.ivy", IVY)
    .config("spark.sql.extensions", "org.apache.iceberg.spark.extensions.IcebergSparkSessionExtensions")
    .config("spark.sql.catalog.wd", "org.apache.iceberg.spark.SparkCatalog")
    .config("spark.sql.catalog.wd.type", "rest")
    .config("spark.sql.catalog.wd.uri", REST)
    .config("spark.sql.catalog.wd.io-impl", "org.apache.iceberg.aws.s3.S3FileIO")
    .config("spark.sql.catalog.wd.s3.endpoint", WD_S3)
    .config("spark.sql.catalog.wd.s3.path-style-access", "true")
    .config("spark.sql.catalog.wd.s3.access-key-id", "adminkey")
    .config("spark.sql.catalog.wd.s3.secret-access-key", "adminsecret")
    .config("spark.sql.catalog.wd.client.region", "us-east-1")
    .config("spark.ui.enabled", "false")
    .getOrCreate()
)
spark.sparkContext.setLogLevel("ERROR")

QUERY = "SELECT sum(c0) FROM wd.bench.{t}"   # 1 of 32 columns -> projection pushdown

def bench(table):
    times, byts = [], []
    for i in range(RUNS):
        b0 = read_bytes()
        t0 = time.time()
        spark.sql(QUERY.format(t=table)).collect()
        dt = (time.time() - t0) * 1000.0
        db = read_bytes() - b0
        if i >= WARMUP:
            times.append(dt); byts.append(db)
    return statistics.median(times), statistics.median(byts)

print(f"\n1-column aggregate (sum(c0)), median of {RUNS - WARMUP} runs:\n")
print(f"  {'table':12s} {'wall_ms':>10s} {'bytes_read':>12s}")
res = {}
for t in ("plain", "placed"):
    ms, by = bench(t); res[t] = (ms, by)
    print(f"  {t:12s} {ms:>10.1f} {by:>12d}")
pm, pb = res["plain"]; cm, cb = res["placed"]
print(f"\nbytes read:  placed reads {pb / max(cb,1):.1f}x less than plain  (deployment-independent)")
print(f"wall-clock:  placed {cm:.0f}ms vs plain {pm:.0f}ms = {pm / max(cm,1e-9):.2f}x "
      f"({'faster' if cm < pm else 'no faster'} on this loopback/warm-cache rig)")
spark.stop()
