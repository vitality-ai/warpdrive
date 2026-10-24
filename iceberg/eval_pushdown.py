#!/usr/bin/env python3
"""Layer 3 (direct, S3-level): intra-object pushdown via content-dependent
placement of a Parquet file's columns.

Stores one Parquet two ways in WarpDrive and measures bytes read off the shard
backing store for a single-column projection (the honest read-amplification
metric, reported by the server as `x-warpd-shard-bytes-read`):

  * plain       -- explicit opt-out (`x-warpd-computable-units: false`): one
                   opaque blob split into K positional shards; a range still
                   reads every shard it spans.
  * content-dep -- each column chunk is split into K contiguous sub-units all
                   tagged with the column index; the `ivf_centroid` group-by-key
                   packer places a column's K sub-units in ONE stripe (one per
                   data bin => balanced, ~parity-only overhead) and isolates the
                   column to that stripe. The footer gets its own tiny stripe.

A 1-column projection then touches only that column's stripe (+ the footer
stripe) instead of dragging whole positional shards.

Prereq: a distributed WarpDrive cluster (default http://127.0.0.1:9760) with a
`warehouse` bucket configured for the grouping packer and a generous overhead
budget (the K-split layout actually needs ~0%, but the whole-object no-header
fallback is what the budget guards):
    PUT /cluster/_admin/bucket_config/warehouse
        {"packer_name":"ivf_centroid","overhead_threshold_pct":500.0}
Run: venv/bin/python eval_pushdown.py
"""
import io, os, json, base64, struct
import boto3, pyarrow as pa, pyarrow.parquet as pq
from botocore.config import Config

WD_S3 = os.environ.get("WD_S3", "http://127.0.0.1:9760")
B = os.environ.get("WD_WAREHOUSE_BUCKET", "warehouse")
K = int(os.environ.get("WD_RS_K", "3"))
FRAMING = 1 << 30  # a group id no real column index will collide with

s3 = boto3.client("s3", endpoint_url=WD_S3, aws_access_key_id="adminkey",
                  aws_secret_access_key="adminsecret", region_name="us-east-1",
                  config=Config(s3={"addressing_style": "path"}))


def make_parquet():
    N = 60000
    cols = {f"c{i}": pa.array([(i * 1000 + j) if i % 2 else float(j) * 1.5 for j in range(N)]) for i in range(8)}
    buf = io.BytesIO()
    pq.write_table(pa.table(cols), buf, row_group_size=N, compression="zstd")  # single row group
    return buf.getvalue()


def column_units(data):
    """Exact tiling of the file: each column chunk split K ways (group = column
    index), gaps/footer as framing units. Returns the units-header list."""
    pf = pq.ParquetFile(io.BytesIO(data)); md = pf.metadata
    assert md.num_row_groups == 1
    rg = md.row_group(0)
    chunks = []
    for c in range(rg.num_columns):
        col = rg.column(c)
        start = col.data_page_offset
        if col.dictionary_page_offset and col.dictionary_page_offset > 0:
            start = min(start, col.dictionary_page_offset)
        chunks.append((c, start, col.total_compressed_size))
    chunks.sort(key=lambda x: x[1])

    units = []
    def add(off, length, group):
        if length <= 0:
            return
        meta = base64.b64encode(struct.pack("<I", group)).decode()
        units.append([off, length, length, "opaque", meta])
    def add_split(off, length, group):
        base = length // K
        for i in range(K):
            add(off + i * base, base if i < K - 1 else length - base * (K - 1), group)

    cur = 0
    for (c, start, length) in chunks:
        if start > cur:
            add_split(cur, start - cur, FRAMING)
        add_split(start, length, c)
        cur = max(cur, start + length)
    if cur < len(data):
        add_split(cur, len(data) - cur, FRAMING)
    assert sum(u[1] for u in units) == len(data), "tiling must cover the whole file"
    return units, chunks


def put(key, data, header_val):
    evt = "before-sign.s3.PutObject"
    def inj(request, **kw):
        if header_val is not None:
            request.headers["x-warpd-computable-units"] = header_val
    s3.meta.events.register(evt, inj)
    try:
        h = s3.put_object(Bucket=B, Key=key, Body=data)["ResponseMetadata"]["HTTPHeaders"]
        return h.get("x-warpd-placement", "?"), h.get("x-warpd-pack-overhead-pct", "?")
    finally:
        s3.meta.events.unregister(evt, inj)


def ranged(key, start, end):
    r = s3.get_object(Bucket=B, Key=key, Range=f"bytes={start}-{end}")
    h = r["ResponseMetadata"]["HTTPHeaders"]; r["Body"].read()
    return int(h.get("x-warpd-shard-bytes-read", -1)), h.get("x-warpd-stripes-fetched"), h.get("x-warpd-stripes-total")


def main():
    data = make_parquet(); size = len(data)
    units, chunks = column_units(data)
    print(f"parquet size={size} cols={len(chunks)} units={len(units)} (exact tiling)")

    pl, _ = put("pushdown/plain.parquet", data, "false")                 # true plain baseline
    cd, ov = put("pushdown/placed.parquet", data, json.dumps(units))     # column-grouped
    print(f"stored: plain={pl}, content-dependent={cd} (pack-overhead={ov}%)")
    assert cd == "packed", "content-dependent PUT did not engage (check bucket config / overhead budget)"
    assert s3.get_object(Bucket=B, Key="pushdown/placed.parquet")["Body"].read() == data, "round-trip corrupt"
    print("content-dependent full GET byte-identical to original: OK")

    footer_start = chunks[-1][1] + chunks[-1][2]
    c0_start, c0_len = chunks[0][1], chunks[0][2]
    print("\n1-column projection (column 0 + footer):")
    results = {}
    for key, label in [("pushdown/plain.parquet", "plain"), ("pushdown/placed.parquet", "content-dep")]:
        b_col, sf, st = ranged(key, c0_start, c0_start + c0_len - 1)
        b_foot, _, _ = ranged(key, footer_start, size - 1)
        results[label] = b_col + b_foot
        print(f"  [{label:11s}] column={b_col:>9}  footer={b_foot:>7}  total={b_col + b_foot:>9}  stripes={sf}/{st}")
    speedup = results["plain"] / results["content-dep"]
    print(f"\nobject size={size}; intra-object pushdown read {speedup:.1f}x less off disk "
          f"for a 1-of-{len(chunks)}-column projection (overhead {ov}%).")
    assert speedup > 2.0, f"expected a clear pushdown win, got {speedup:.1f}x"
    print("PASS: Layer 3 intra-object pushdown (content-dependent column placement)")


if __name__ == "__main__":
    main()
