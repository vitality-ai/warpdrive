#!/usr/bin/env python3
"""Run real TPC-H queries (Q6, Q1) in Spark against lineitem stored plain vs
column-placed on WarpDrive, timing wall-clock and the server shard-bytes-read
delta per query. Run tpch_build.py first.
Run: SPARK_LOCAL_IP=127.0.0.1 venv/bin/python tpch_spark.py
"""
import os, time, json, statistics, urllib.request
from pyspark.sql import SparkSession

WD_S3 = os.environ.get("WD_S3", "http://127.0.0.1:9760")
REST = os.environ.get("REST", "http://127.0.0.1:8181")
IVY = "/tmp/claude-0/-home-user/1092b597-fec1-566d-bf89-445018d387d4/scratchpad/ivy"
RUNS, WARMUP = int(os.environ.get("RUNS", "7")), 2

# TPC-H Q6 (4 of 16 cols) and Q1 (7 of 16 cols) -- {t} is the table.
Q6 = """SELECT sum(l_extendedprice*l_discount) AS revenue FROM wd.tpch.{t}
        WHERE l_shipdate >= DATE '1994-01-01' AND l_shipdate < DATE '1995-01-01'
          AND l_discount BETWEEN 0.05 AND 0.07 AND l_quantity < 24"""
Q1 = """SELECT l_returnflag, l_linestatus, sum(l_quantity) sq, sum(l_extendedprice) sp,
               sum(l_extendedprice*(1-l_discount)) dp,
               sum(l_extendedprice*(1-l_discount)*(1+l_tax)) cp, count(*) c
        FROM wd.tpch.{t} WHERE l_shipdate <= DATE '1998-09-02'
        GROUP BY l_returnflag, l_linestatus ORDER BY l_returnflag, l_linestatus"""

def read_bytes():
    with urllib.request.urlopen(f"{WD_S3}/cluster/_internal/read_stats", timeout=5) as r:
        return json.load(r)["shard_bytes_read"]

spark = (
    SparkSession.builder.appName("tpch").master("local[2]")
    .config("spark.jars.packages", "org.apache.iceberg:iceberg-spark-runtime-4.0_2.13:1.12.0,org.apache.iceberg:iceberg-aws-bundle:1.12.0")
    .config("spark.jars.ivy", IVY)
    .config("spark.sql.extensions", "org.apache.iceberg.spark.extensions.IcebergSparkSessionExtensions")
    .config("spark.sql.catalog.wd", "org.apache.iceberg.spark.SparkCatalog")
    .config("spark.sql.catalog.wd.type", "rest").config("spark.sql.catalog.wd.uri", REST)
    .config("spark.sql.catalog.wd.io-impl", "org.apache.iceberg.aws.s3.S3FileIO")
    .config("spark.sql.catalog.wd.s3.endpoint", WD_S3)
    .config("spark.sql.catalog.wd.s3.path-style-access", "true")
    .config("spark.sql.catalog.wd.s3.access-key-id", "adminkey")
    .config("spark.sql.catalog.wd.s3.secret-access-key", "adminsecret")
    .config("spark.sql.catalog.wd.client.region", "us-east-1")
    .config("spark.ui.enabled", "false").getOrCreate()
)
spark.sparkContext.setLogLevel("ERROR")

def bench(q, table):
    times, byts = [], []
    for i in range(RUNS):
        b0 = read_bytes(); t0 = time.time()
        spark.sql(q.format(t=table)).collect()
        dt = (time.time()-t0)*1000; db = read_bytes()-b0
        if i >= WARMUP: times.append(dt); byts.append(db)
    return statistics.median(times), statistics.median(byts)

print(f"\nTPC-H on lineitem SF=1 (6M rows, 16 cols), median of {RUNS-WARMUP} runs:\n")
print(f"  {'query':5s} {'table':9s} {'wall_ms':>9s} {'MB_read':>9s}")
for qn, q in [("Q6", Q6), ("Q1", Q1)]:
    r = {}
    for t in ("lineitem_plain", "lineitem_placed"):
        ms, by = bench(q, t); r[t] = (ms, by)
        print(f"  {qn:5s} {t.split('_')[1]:9s} {ms:>9.1f} {by/1e6:>9.1f}")
    pm, pb = r["lineitem_plain"]; cm, cb = r["lineitem_placed"]
    print(f"        -> placed: {pb/max(cb,1):.1f}x less read, {pm/max(cm,1e-9):.2f}x wall-clock\n")
spark.stop()
