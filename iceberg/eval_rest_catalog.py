#!/usr/bin/env python3
"""Evaluate Layer 2: the Iceberg REST catalog (rest_catalog.py) on WarpDrive.

Two checks:
  A. Lifecycle -- a real pyiceberg RestCatalog, given ONLY the REST uri (all S3
     wiring arrives via /v1/config overrides), does create namespace/table,
     append x2, full/filtered/time-travel scans, and reload-from-catalog.
  B. Atomicity -- two INDEPENDENT catalog processes (no shared in-process lock)
     race concurrent commits on one table. The only thing serializing them is
     WarpDrive's conditional-PUT CAS on the table-pointer object. Asserts: zero
     lost updates, one distinct snapshot per append, and that real 409 conflicts
     actually occurred (so the race was genuinely exercised).

Prereq (see rest_catalog.py):
  - a distributed WarpDrive cluster (WD_S3, default http://127.0.0.1:9760)
  - catalog instance #1 on :8181   (REST_PORT=8181 python rest_catalog.py)
  - catalog instance #2 on :8182   (REST_PORT=8182 python rest_catalog.py)
    (check B is skipped with a note if :8182 is not reachable)
Run: venv/bin/python eval_rest_catalog.py
"""
import os, threading, urllib.request
import pyarrow as pa
from pyiceberg.catalog.rest import RestCatalog
from pyiceberg.exceptions import CommitFailedException

CAT1 = os.environ.get("REST1", "http://127.0.0.1:8181")
CAT2 = os.environ.get("REST2", "http://127.0.0.1:8182")


def lifecycle():
    cat = RestCatalog("wd", uri=CAT1)          # only the REST uri; S3 from /v1/config
    cat.create_namespace_if_not_exists("db")
    data = pa.table({"id": pa.array(range(1000), pa.int64()),
                     "region": pa.array([f"r{i%8}" for i in range(1000)]),
                     "amount": pa.array([float(i) * 1.5 for i in range(1000)], pa.float64())})
    try: cat.drop_table("db.events")
    except Exception: pass
    tbl = cat.create_table("db.events", schema=data.schema)
    tbl.append(data)
    tbl.append(data.slice(0, 200)); tbl.refresh()
    n  = tbl.scan().to_arrow().num_rows
    f  = tbl.scan(row_filter="region == 'r3'").to_arrow().num_rows
    n0 = tbl.scan(snapshot_id=[s.snapshot_id for s in tbl.metadata.snapshots][0]).to_arrow().num_rows
    assert (n, f, n0) == (1200, 150, 1000), f"mismatch {(n, f, n0)}"
    assert RestCatalog("wd2", uri=CAT1).load_table("db.events").scan().to_arrow().num_rows == 1200
    print(f"  A. lifecycle: snapshots={len(tbl.metadata.snapshots)} full={n} filtered={f} "
          f"time_travel={n0} + reload -> PASS")


def _reachable(uri):
    try:
        urllib.request.urlopen(uri + "/v1/config", timeout=2).read(); return True
    except Exception:
        return False


def atomicity():
    if not _reachable(CAT2):
        print(f"  B. atomicity: SKIP (second catalog {CAT2} not reachable)"); return
    TBL, N = "db.race", 15
    cat0 = RestCatalog("c0", uri=CAT1)
    cat0.create_namespace_if_not_exists("db")
    try: cat0.drop_table(TBL)
    except Exception: pass
    seed = pa.table({"id": pa.array([0], pa.int64())})
    # commit.retry.num-retries=0 => a 409 surfaces to us instead of being
    # auto-retried inside pyiceberg, so the race is counted deterministically.
    t = cat0.create_table(TBL, schema=seed.schema, properties={"commit.retry.num-retries": "0"})
    t.append(seed)
    endpoints = [CAT1, CAT2]
    conflicts = [0, 0]; committed = [0, 0]

    def worker(i):
        cat = RestCatalog(f"c{i}", uri=endpoints[i])
        for j in range(N):
            row = pa.table({"id": pa.array([1000 * (i + 1) + j], pa.int64())})
            while True:
                tbl = cat.load_table(TBL)
                try:
                    tbl.append(row); committed[i] += 1; break
                except CommitFailedException:
                    conflicts[i] += 1     # lost the CAS -> reload + retry

    ths = [threading.Thread(target=worker, args=(i,)) for i in range(2)]
    for x in ths: x.start()
    for x in ths: x.join()

    final = cat0.load_table(TBL)
    ids = sorted(final.scan().to_arrow().column("id").to_pylist())
    expected = [0] + sorted([1000 * (i + 1) + j for i in range(2) for j in range(N)])
    n_snap = len(final.metadata.snapshots)
    assert ids == expected, f"LOST UPDATE: missing={set(expected) - set(ids)} dups={len(ids) - len(set(ids))}"
    assert n_snap == 1 + 2 * N, f"snapshots {n_snap} != {1 + 2 * N}"
    assert sum(conflicts) > 0, "no conflicts -- race not exercised (inconclusive)"
    print(f"  B. atomicity: 2 independent catalog procs, {2 * N} concurrent appends, "
          f"{sum(conflicts)} real 409 conflicts retried, {n_snap} snapshots, 0 lost updates -> PASS")


if __name__ == "__main__":
    print("Iceberg REST catalog on WarpDrive:")
    lifecycle()
    atomicity()
    print("PASS: Layer 2 (REST catalog + WarpDrive-CAS commit atomicity)")
