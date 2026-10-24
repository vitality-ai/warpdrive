#!/usr/bin/env python3
"""Evaluate Iceberg compatibility: a real pyiceberg client against a live
DISTRIBUTED WarpDrive (RS(3,2)) via the authenticated /cluster/s3 surface.

Findings this encodes:
  - Use the fsspec/s3fs FileIO, NOT pyiceberg's default pyarrow FileIO:
    pyarrow's S3FileSystem takes only host:port and cannot target WarpDrive's
    /cluster/s3 path prefix. boto3/s3fs accept a path-prefixed endpoint.
  - The distributed coordinator decodes aws-chunked bodies; the single-node
    /s3 path does not (it stored chunk framing into objects -> corrupt reads),
    so Iceberg must run against the distributed surface.
  - Catalog = local SqlCatalog (atomic commit in SQLite), so the S3 layer only
    needs PUT/GET/HEAD/LIST/DELETE, not S3 conditional-write commit semantics.

Prereq: a cluster started with admin creds, warehouse bucket created on
/cluster/s3. Run: venv/bin/python eval_pyiceberg.py
"""
import os, pyarrow as pa
from pyiceberg.catalog.sql import SqlCatalog

ENDPOINT = os.environ.get("WD_S3", "http://127.0.0.1:9710/cluster/s3")
DB = os.environ.get("CAT_DB", "/tmp/wd-iceberg-cat.db")
try: os.remove(DB)
except FileNotFoundError: pass

cat = SqlCatalog("wd", **{
    "uri": f"sqlite:///{DB}", "warehouse": "s3://warehouse/",
    "py-io-impl": "pyiceberg.io.fsspec.FsspecFileIO",
    "s3.endpoint": ENDPOINT,
    "s3.access-key-id": os.environ.get("WD_KEY", "adminkey"),
    "s3.secret-access-key": os.environ.get("WD_SECRET", "adminsecret"),
    "s3.region": "us-east-1", "s3.path-style-access": "true",
})
cat.create_namespace_if_not_exists("db")
data = pa.table({"id": pa.array(range(1000), pa.int64()),
                 "region": pa.array([f"r{i%8}" for i in range(1000)]),
                 "amount": pa.array([float(i)*1.5 for i in range(1000)], pa.float64())})
tbl = cat.create_table_if_not_exists("db.events", schema=data.schema)
tbl.append(data)
tbl.append(data.slice(0, 200)); tbl.refresh()
n  = tbl.scan().to_arrow().num_rows
f  = tbl.scan(row_filter="region == 'r3'").to_arrow().num_rows
n0 = tbl.scan(snapshot_id=[s.snapshot_id for s in tbl.metadata.snapshots][0]).to_arrow().num_rows
print(f"snapshots={len(tbl.metadata.snapshots)} full={n} filtered={f} time_travel={n0}")
assert (n, f, n0) == (1200, 150, 1000), "mismatch"
print("PASS: Iceberg table (create/append/snapshot/filter/time-travel) on distributed WarpDrive")
