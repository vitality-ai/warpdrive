#!/usr/bin/env python3
"""Layer 3, end-to-end THROUGH Iceberg: content-dependent column placement of a
real Iceberg table's data files, measured through pyiceberg's own scan.

Bridge used here is "place-after-write": write the table normally through the
REST catalog (pyarrow writes plain Parquet data files), then rewrite each data
file BYTE-IDENTICALLY to the same S3 key with per-column units (each column
chunk split K ways under the column's group id). Identical bytes => Iceberg's
metadata (path, size, record count) is untouched and the table is unchanged to
any reader; only WarpDrive's physical placement of the file changes. This keeps
the bridge out of the client entirely (no custom FileIO), which is why the exact
same pyiceberg scan below benefits with zero engine changes.

Measurement is end to end: the server keeps a cumulative shard-bytes-read counter
(/cluster/_internal/read_stats); we snapshot it around a real pyiceberg scan, so
the number is what pyiceberg's own planner+FileIO actually pulled off disk, not a
replayed approximation.

Compares a projection (one column) on a PLACED table vs an unplaced (plain)
table, and projection vs full scan on the placed table.

Prereq: cluster on WD_S3 (default :9760) with `warehouse` bucket configured
packer=ivf_centroid, overhead_threshold_pct=50 (so the plain table's
no-header writes fall back to plain and the placed writes still engage); a REST
catalog on REST (default :8181). Run: venv/bin/python eval_pushdown_iceberg.py
"""
import io, os, json, base64, struct, urllib.request
import boto3, pyarrow as pa, pyarrow.parquet as pq
from botocore.config import Config
from pyiceberg.catalog.rest import RestCatalog

WD_S3 = os.environ.get("WD_S3", "http://127.0.0.1:9760")
REST  = os.environ.get("REST", "http://127.0.0.1:8181")
WH    = os.environ.get("WD_WAREHOUSE_BUCKET", "warehouse")
K     = int(os.environ.get("WD_RS_K", "3"))
FRAMING = 1 << 30

s3 = boto3.client("s3", endpoint_url=WD_S3, aws_access_key_id="adminkey",
                  aws_secret_access_key="adminsecret", region_name="us-east-1",
                  config=Config(s3={"addressing_style": "path"}))

def read_stats():
    with urllib.request.urlopen(f"{WD_S3}/cluster/_internal/read_stats", timeout=5) as r:
        return json.load(r)["shard_bytes_read"]

def s3_split(loc):  # s3://bucket/key -> (bucket, key)
    p = loc[5:]; i = p.index("/"); return p[:i], p[i+1:]

def column_units(data):
    """Per-(row-group,column) chunk, split K ways, grouped by COLUMN index so all
    of a column's chunks across row groups land together. Exact tiling."""
    md = pq.ParquetFile(io.BytesIO(data)).metadata
    chunks = []
    for rg in range(md.num_row_groups):
        r = md.row_group(rg)
        for c in range(r.num_columns):
            col = r.column(c)
            start = col.data_page_offset
            if col.dictionary_page_offset and col.dictionary_page_offset > 0:
                start = min(start, col.dictionary_page_offset)
            chunks.append((c, start, col.total_compressed_size))
    chunks.sort(key=lambda x: x[1])
    units = []
    def add(off, length, group):
        if length > 0:
            units.append([off, length, length, "opaque", base64.b64encode(struct.pack("<I", group)).decode()])
    def add_split(off, length, group):
        base = length // K
        for i in range(K):
            add(off + i * base, base if i < K - 1 else length - base * (K - 1), group)
    cur = 0
    for (c, start, length) in chunks:
        if start > cur: add_split(cur, start - cur, FRAMING)
        add_split(start, length, c)
        cur = max(cur, start + length)
    if cur < len(data): add_split(cur, len(data) - cur, FRAMING)
    assert sum(u[1] for u in units) == len(data)
    return units

def place_file(loc):
    """Rewrite one data file byte-identically with column units (content-dependent)."""
    bucket, key = s3_split(loc)
    data = s3.get_object(Bucket=bucket, Key=key)["Body"].read()
    units = column_units(data)
    evt = "before-sign.s3.PutObject"
    def inj(request, **kw): request.headers["x-warpd-computable-units"] = json.dumps(units)
    s3.meta.events.register(evt, inj)
    try:
        pl = s3.put_object(Bucket=bucket, Key=key, Body=data)["ResponseMetadata"]["HTTPHeaders"].get("x-warpd-placement")
    finally:
        s3.meta.events.unregister(evt, inj)
    assert s3.get_object(Bucket=bucket, Key=key)["Body"].read() == data, f"placement corrupted {key}"
    return pl

def build_table(cat, name):
    cat.create_namespace_if_not_exists("bench")
    data = pa.table({f"c{i}": pa.array([(i * 1000 + j) if i % 2 else float(j) * 1.5 for j in range(60000)])
                     for i in range(8)})
    try: cat.drop_table(f"bench.{name}")
    except Exception: pass
    t = cat.create_table(f"bench.{name}", schema=data.schema)
    t.append(data)
    return t

def scan_bytes(cat, name, fields):
    t = cat.load_table(f"bench.{name}")
    before = read_stats()
    rows = (t.scan(selected_fields=fields) if fields else t.scan()).to_arrow().num_rows
    return read_stats() - before, rows

def main():
    cat = RestCatalog("wd", uri=REST)
    # plain table (no placement: writes have no units header -> fall back to plain)
    tp = build_table(cat, "plain")
    # placed table: same data, then rewrite each data file by column
    tc = build_table(cat, "placed")
    files = [ft.file.file_path for ft in tc.scan().plan_files()]
    placements = [place_file(f) for f in files]
    print(f"placed {len(files)} data file(s): placement={set(placements)}")
    assert placements and all(p == "packed" for p in placements), "data files did not become content-dependent"

    proj_plain, n1  = scan_bytes(cat, "plain",  ("c0",))
    proj_placed, n2 = scan_bytes(cat, "placed", ("c0",))
    full_placed, n3 = scan_bytes(cat, "placed", None)
    print(f"\nrows: plain={n1} placed(proj)={n2} placed(full)={n3}")
    print(f"1-column projection, shard bytes read off disk (via pyiceberg's own scan):")
    print(f"  plain  table : {proj_plain:>10}")
    print(f"  placed table : {proj_placed:>10}   ({proj_plain / max(proj_placed,1):.1f}x less)")
    print(f"  placed full  : {full_placed:>10}   (projection reads {full_placed / max(proj_placed,1):.1f}x less than full)")
    assert n1 == n2 and proj_placed > 0
    assert proj_plain / proj_placed > 1.5, f"expected pushdown win, got {proj_plain/proj_placed:.1f}x"
    assert full_placed > proj_placed, "projection should read less than full scan"
    print("PASS: Layer 3 intra-object pushdown through a real pyiceberg scan")

if __name__ == "__main__":
    main()
