#!/usr/bin/env python3
"""Evaluate Iceberg compatibility: a real pyiceberg client against a live
DISTRIBUTED WarpDrive (RS(3,2)) via the authenticated /cluster/s3 surface.

Findings this encodes:
  - BOTH FileIO backends work: pyiceberg's default pyarrow FileIO
    (PyArrowFileIO) and the fsspec/s3fs FileIO. The pyarrow backend used to
    fail with "Response checksums mismatch": it uploads via streaming-trailer
    multipart declaring CRC64NVME and validates that same algorithm on its
    ranged Parquet reads, but the coordinator hardcoded CRC32C on write and
    echoed the whole-object checksum even on partial (206) responses. Both are
    fixed (see the "honor client checksum algorithm end-to-end" commit), so no
    checksum workaround or env var is needed. This eval runs both to prove it.
  - Root-path S3 routing: object ops under `/{bucket}/{key}` are served by the
    distributed coordinator in cluster mode, so a plain host:port endpoint
    (what pyarrow's S3FileSystem accepts) reaches the distributed surface --
    no /cluster/s3 path prefix required.
  - The distributed coordinator decodes aws-chunked bodies; the single-node
    /s3 path does not (it stored chunk framing into objects -> corrupt reads),
    so Iceberg must run against the distributed surface.
  - Catalog = local SqlCatalog (atomic commit in SQLite), so the S3 layer only
    needs PUT/GET/HEAD/LIST/DELETE, not S3 conditional-write commit semantics.

Prereq: a cluster started with admin creds, warehouse bucket created.
Run: venv/bin/python eval_pyiceberg.py
"""
import os, pyarrow as pa
from pyiceberg.catalog.sql import SqlCatalog

ENDPOINT = os.environ.get("WD_S3", "http://127.0.0.1:9710")


def run(io_impl, ns):
    """Create/append/snapshot/filter/time-travel against one FileIO backend."""
    db = f"/tmp/wd-iceberg-cat-{ns}.db"
    try: os.remove(db)
    except FileNotFoundError: pass
    cat = SqlCatalog("wd", **{
        "uri": f"sqlite:///{db}", "warehouse": "s3://warehouse/",
        "py-io-impl": io_impl,
        "s3.endpoint": ENDPOINT,
        "s3.access-key-id": os.environ.get("WD_KEY", "adminkey"),
        "s3.secret-access-key": os.environ.get("WD_SECRET", "adminsecret"),
        "s3.region": "us-east-1", "s3.path-style-access": "true",
    })
    cat.create_namespace_if_not_exists(ns)
    data = pa.table({"id": pa.array(range(1000), pa.int64()),
                     "region": pa.array([f"r{i%8}" for i in range(1000)]),
                     "amount": pa.array([float(i)*1.5 for i in range(1000)], pa.float64())})
    tbl = cat.create_table_if_not_exists(f"{ns}.events", schema=data.schema)
    tbl.append(data)
    tbl.append(data.slice(0, 200)); tbl.refresh()
    n  = tbl.scan().to_arrow().num_rows
    f  = tbl.scan(row_filter="region == 'r3'").to_arrow().num_rows
    n0 = tbl.scan(snapshot_id=[s.snapshot_id for s in tbl.metadata.snapshots][0]).to_arrow().num_rows
    print(f"  [{io_impl.rsplit('.',1)[-1]}] snapshots={len(tbl.metadata.snapshots)} "
          f"full={n} filtered={f} time_travel={n0}")
    assert (n, f, n0) == (1200, 150, 1000), "mismatch"


run("pyiceberg.io.pyarrow.PyArrowFileIO", "db_pyarrow")
run("pyiceberg.io.fsspec.FsspecFileIO", "db_fsspec")
print("PASS: Iceberg (create/append/snapshot/filter/time-travel) on distributed "
      "WarpDrive via BOTH pyarrow and fsspec FileIO -- no checksum workaround")
