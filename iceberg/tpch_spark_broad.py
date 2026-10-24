#!/usr/bin/env python3
"""Broader TPC-H A/B in Spark: Q1, Q3, Q5, Q6, Q9, Q18 and a wide all-columns
scan, each run on the plain and the column-placed copy of the schema. For each
query: assert placed and plain return identical results (placement is transparent
across joins), then report median wall-clock + server bytes-read delta. Shows the
HONEST range -- where pushdown wins big, a little, or not at all.
Run tpch_load_all.py first. Run: SPARK_LOCAL_IP=127.0.0.1 venv/bin/python tpch_spark_broad.py
"""
import os, time, json, statistics, urllib.request
from pyspark.sql import SparkSession

WD_S3 = os.environ.get("WD_S3", "http://127.0.0.1:9760")
REST = os.environ.get("REST", "http://127.0.0.1:8181")
IVY = "/tmp/claude-0/-home-user/1092b597-fec1-566d-bf89-445018d387d4/scratchpad/ivy"
RUNS, WARMUP = int(os.environ.get("RUNS", "4")), 1

Q = {
"Q1 (1 tbl, 7 cols)": """SELECT l_returnflag,l_linestatus,sum(l_quantity),sum(l_extendedprice),
  sum(l_extendedprice*(1-l_discount)),sum(l_extendedprice*(1-l_discount)*(1+l_tax)),count(*)
  FROM wd.tpch.lineitem_{v} WHERE l_shipdate<=DATE '1998-09-02'
  GROUP BY l_returnflag,l_linestatus ORDER BY l_returnflag,l_linestatus""",
"Q6 (1 tbl, 4 cols)": """SELECT sum(l_extendedprice*l_discount) FROM wd.tpch.lineitem_{v}
  WHERE l_shipdate>=DATE '1994-01-01' AND l_shipdate<DATE '1995-01-01'
  AND l_discount BETWEEN 0.05 AND 0.07 AND l_quantity<24""",
"Q3 (3-tbl join)": """SELECT l_orderkey,sum(l_extendedprice*(1-l_discount)) rev,o_orderdate,o_shippriority
  FROM wd.tpch.customer_{v},wd.tpch.orders_{v},wd.tpch.lineitem_{v}
  WHERE c_mktsegment='BUILDING' AND c_custkey=o_custkey AND l_orderkey=o_orderkey
  AND o_orderdate<DATE '1995-03-15' AND l_shipdate>DATE '1995-03-15'
  GROUP BY l_orderkey,o_orderdate,o_shippriority ORDER BY rev DESC,o_orderdate LIMIT 10""",
"Q5 (6-tbl join)": """SELECT n_name,sum(l_extendedprice*(1-l_discount)) rev
  FROM wd.tpch.customer_{v},wd.tpch.orders_{v},wd.tpch.lineitem_{v},wd.tpch.supplier_{v},
       wd.tpch.nation_{v},wd.tpch.region_{v}
  WHERE c_custkey=o_custkey AND l_orderkey=o_orderkey AND l_suppkey=s_suppkey
  AND c_nationkey=s_nationkey AND s_nationkey=n_nationkey AND n_regionkey=r_regionkey
  AND r_name='ASIA' AND o_orderdate>=DATE '1994-01-01' AND o_orderdate<DATE '1995-01-01'
  GROUP BY n_name ORDER BY rev DESC""",
"Q9 (6-tbl join)": """SELECT nation,o_year,sum(amount) FROM (
  SELECT n_name nation,year(o_orderdate) o_year,
    l_extendedprice*(1-l_discount)-ps_supplycost*l_quantity amount
  FROM wd.tpch.part_{v},wd.tpch.supplier_{v},wd.tpch.lineitem_{v},wd.tpch.partsupp_{v},
       wd.tpch.orders_{v},wd.tpch.nation_{v}
  WHERE s_suppkey=l_suppkey AND ps_suppkey=l_suppkey AND ps_partkey=l_partkey
    AND p_partkey=l_partkey AND o_orderkey=l_orderkey AND s_nationkey=n_nationkey
    AND p_name LIKE '%green%') p GROUP BY nation,o_year ORDER BY nation,o_year DESC""",
"Q18 (3-tbl+subq)": """SELECT c_name,c_custkey,o_orderkey,o_orderdate,o_totalprice,sum(l_quantity)
  FROM wd.tpch.customer_{v},wd.tpch.orders_{v},wd.tpch.lineitem_{v}
  WHERE o_orderkey IN (SELECT l_orderkey FROM wd.tpch.lineitem_{v} GROUP BY l_orderkey HAVING sum(l_quantity)>300)
  AND c_custkey=o_custkey AND o_orderkey=l_orderkey
  GROUP BY c_name,c_custkey,o_orderkey,o_orderdate,o_totalprice ORDER BY o_totalprice DESC,o_orderdate LIMIT 100""",
"WIDE (1 tbl, 16 cols)": """SELECT count(*),sum(l_quantity),sum(l_extendedprice),sum(l_discount),sum(l_tax),
  sum(l_partkey),sum(l_suppkey),sum(l_orderkey),sum(l_linenumber),max(l_returnflag),max(l_linestatus),
  max(l_shipdate),max(l_commitdate),max(l_receiptdate),max(l_shipinstruct),max(l_shipmode),max(l_comment)
  FROM wd.tpch.lineitem_{v}""",
}

def read_bytes():
    with urllib.request.urlopen(f"{WD_S3}/cluster/_internal/read_stats", timeout=5) as r:
        return json.load(r)["shard_bytes_read"]

spark = (SparkSession.builder.appName("tpch-broad").master("local[2]")
    .config("spark.jars.packages","org.apache.iceberg:iceberg-spark-runtime-4.0_2.13:1.12.0,org.apache.iceberg:iceberg-aws-bundle:1.12.0")
    .config("spark.jars.ivy", IVY)
    .config("spark.sql.extensions","org.apache.iceberg.spark.extensions.IcebergSparkSessionExtensions")
    .config("spark.sql.catalog.wd","org.apache.iceberg.spark.SparkCatalog")
    .config("spark.sql.catalog.wd.type","rest").config("spark.sql.catalog.wd.uri",REST)
    .config("spark.sql.catalog.wd.io-impl","org.apache.iceberg.aws.s3.S3FileIO")
    .config("spark.sql.catalog.wd.s3.endpoint",WD_S3).config("spark.sql.catalog.wd.s3.path-style-access","true")
    .config("spark.sql.catalog.wd.s3.access-key-id","adminkey").config("spark.sql.catalog.wd.s3.secret-access-key","adminsecret")
    .config("spark.sql.catalog.wd.client.region","us-east-1").config("spark.ui.enabled","false").getOrCreate())
spark.sparkContext.setLogLevel("ERROR")

def run(q, v):
    return spark.sql(q.format(v=v)).collect()

def bench(q, v):
    times, byts = [], []
    for i in range(RUNS):
        b0=read_bytes(); t0=time.time(); run(q,v); dt=(time.time()-t0)*1000; db=read_bytes()-b0
        if i>=WARMUP: times.append(dt); byts.append(db)
    return statistics.median(times), statistics.median(byts)

print(f"\nTPC-H SF=1, median of {RUNS-WARMUP} runs. placed vs plain:\n")
print(f"  {'query':22s} {'plain_ms':>9s} {'placed_ms':>9s} {'wall x':>7s} {'read x':>7s} {'ok':>3s}")
for name, q in Q.items():
    ok = (run(q,"plain") == run(q,"placed"))   # identical results => placement transparent
    pm, pb = bench(q,"plain"); cm, cb = bench(q,"placed")
    print(f"  {name:22s} {pm:>9.0f} {cm:>9.0f} {pm/max(cm,1e-9):>6.2f}x {pb/max(cb,1):>6.1f}x {'Y' if ok else 'N':>3s}")
spark.stop()
