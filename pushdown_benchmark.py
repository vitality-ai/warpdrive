"""
Reproduces the shape of Fusion's Figure 13 (ASPLOS'25): per-column median/p99
latency reduction from query pushdown vs. a baseline that reassembles the
whole object before filtering -- measured for real, against the live
WarpDrive cluster, not simulated.

Columns are synthetic float64 arrays of varying compressibility (mirroring
Figure 6's real finding that TPC-H lineitem columns compress anywhere from
~1x to ~63x), zlib-compressed (see pushdown.rs's ZlibF64Codec -- a real,
standard stand-in for Parquet's dictionary/Snappy encoding, chosen so the
Python side needs no dependency beyond the stdlib zlib module).

Baseline path: GET /cluster/{bucket}/{key} (the existing full-object
reconstruction -- fans out to every stripe/peer, EC-decodes, reassembles),
then slice out the one column's bytes client-side and decode+filter it.
This is exactly what Fusion's own baseline does: "assembles the Parquet
object on a coordinator node before executing the query on it."

Pushdown path: POST /cluster/{bucket}/{key}/query -- one hop to the single
peer holding that column's data block, in-situ filter, no reassembly.

Usage: python3 pushdown_benchmark.py <cluster_base_url> [n_queries]
"""
import json
import random
import statistics
import struct
import sys
import time
import urllib.request
import zlib

CLUSTER = sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:9710"
N_QUERIES = int(sys.argv[2]) if len(sys.argv) > 2 else 30
BUCKET = "pushdownbench"
KEY = "lineitem-synthetic"

# 16 columns, like TPC-H lineitem (Table 3) -- compressibility spans a wide
# range (Figure 6: median 9.3x, max 63.5x) by mixing low-cardinality
# (highly repetitive, compresses well) and high-cardinality (near-random,
# compresses poorly) generators, same mechanism real dictionary encoding
# would reward or penalize.
ROWS_PER_COLUMN = 20_000


def make_column(seed: int, cardinality: int) -> list[float]:
    rng = random.Random(seed)
    if cardinality <= 1:
        return [1.0] * ROWS_PER_COLUMN
    pool = [float(rng.randint(0, cardinality - 1)) for _ in range(cardinality)]
    return [rng.choice(pool) for _ in range(ROWS_PER_COLUMN)]


def compress_column(values: list[float]) -> tuple[bytes, int]:
    raw = b"".join(struct.pack("<d", v) for v in values)
    return zlib.compress(raw, level=6), len(raw)


def http_put(url: str, body: bytes, headers: dict) -> None:
    req = urllib.request.Request(url, data=body, headers=headers, method="PUT")
    urllib.request.urlopen(req, timeout=60).read()


def http_get(url: str) -> bytes:
    return urllib.request.urlopen(url, timeout=60).read()


def http_post_json(url: str, payload: dict) -> dict:
    req = urllib.request.Request(
        url, data=json.dumps(payload).encode(), headers={"Content-Type": "application/json"}, method="POST"
    )
    return json.loads(urllib.request.urlopen(req, timeout=60).read())


def build_and_put_object(columns: list[list[float]]) -> list[dict]:
    """Returns per-column metadata (offset, len, uncompressed_len) needed
    for the baseline path's client-side slice+decode."""
    chunks = [compress_column(col) for col in columns]
    body = b"".join(c for c, _ in chunks)
    header = []
    meta = []
    pos = 0
    for compressed, uncompressed_len in chunks:
        header.append([pos, len(compressed), uncompressed_len, "zlib_f64"])
        meta.append({"offset": pos, "len": len(compressed), "uncompressed_len": uncompressed_len})
        pos += len(compressed)

    http_put(
        f"{CLUSTER}/cluster/{BUCKET}/{KEY}",
        body,
        {"x-warpd-computable-units": json.dumps(header), "Content-Type": "application/octet-stream"},
    )
    return meta


def baseline_query(unit_idx: int, threshold: float, meta: list[dict]) -> tuple[float, int]:
    """Full-object GET (reassembles across every peer the object's stripes
    touch), then client-side slice + decode + filter of one column --
    exactly what an existing erasure-coded store without FAC pushdown
    would have to do."""
    t0 = time.perf_counter()
    full = http_get(f"{CLUSTER}/cluster/{BUCKET}/{KEY}")
    m = meta[unit_idx]
    raw = zlib.decompress(full[m["offset"] : m["offset"] + m["len"]])
    values = struct.unpack(f"<{len(raw)//8}d", raw)
    matched = sum(1 for v in values if v < threshold)
    elapsed = time.perf_counter() - t0
    return elapsed, matched


def pushdown_query(unit_idx: int, threshold: float, node_url: str) -> tuple[float, int]:
    t0 = time.perf_counter()
    resp = http_post_json(f"{node_url}/cluster/{BUCKET}/{KEY}/query", {"unit_id": f"u{unit_idx}", "op": "lt", "value": threshold})
    elapsed = time.perf_counter() - t0
    return elapsed, resp["matched_count"]


def pctl(data: list[float], p: float) -> float:
    s = sorted(data)
    idx = min(len(s) - 1, int(len(s) * p))
    return s[idx]


if __name__ == "__main__":
    try:
        urllib.request.urlopen(
            urllib.request.Request(
                f"{CLUSTER}/cluster/_admin/bucket_config/{BUCKET}",
                data=json.dumps({"packer_name": "fac", "overhead_threshold_pct": 500.0}).encode(),
                headers={"Content-Type": "application/json"},
                method="PUT",
            ),
            timeout=30,
        ).read()

        # 16 columns, cardinality sweeping from highly repetitive (compresses
        # well, like TPC-H's l_returnflag/l_linestatus) to near-random
        # (compresses poorly, like a free-text or high-precision float column).
        cardinalities = [2, 3, 5, 10, 20, 50, 100, 500, 1000, 5000, 10000, 20000, ROWS_PER_COLUMN, ROWS_PER_COLUMN,
                          ROWS_PER_COLUMN, ROWS_PER_COLUMN]
        columns = [make_column(seed=i, cardinality=c) for i, c in enumerate(cardinalities)]
        meta = build_and_put_object(columns)

        rec = json.loads(http_get(f"{CLUSTER}/cluster/_internal/content_record/{BUCKET}/{KEY}"))
        print(f"content-dependent placement confirmed: {len(rec['stripes'])} stripe(s), {len(rec['units'])} units\n")

        print(f"{'col':>3} {'card':>6} {'compressib.':>11} {'p50 base(ms)':>13} {'p50 push(ms)':>13} "
              f"{'p99 base(ms)':>13} {'p99 push(ms)':>13} {'p50 reduction':>14} {'p99 reduction':>14} {'push_down?':>10}")

        node_urls = [f"http://127.0.0.1:{9710+i}" for i in range(5)]

        for i, col in enumerate(columns):
            threshold = statistics.median(col)  # ~50% selectivity, like the paper's default 1% sweep varies
            compress_ratio = meta[i]["uncompressed_len"] / meta[i]["len"]

            base_lat = []
            push_lat = []
            last_resp_pushed = None
            for q in range(N_QUERIES):
                node = node_urls[q % len(node_urls)]  # round-robin coordinator, like the paper's 10 clients
                bl, bm = baseline_query(i, threshold, meta)
                base_lat.append(bl * 1000)
                pl, pm = pushdown_query(i, threshold, node)
                push_lat.append(pl * 1000)
                assert bm == pm, f"correctness mismatch col {i}: baseline={bm} pushdown={pm}"

            resp = http_post_json(f"{CLUSTER}/cluster/{BUCKET}/{KEY}/query", {"unit_id": f"u{i}", "op": "lt", "value": threshold})
            last_resp_pushed = resp["projection_pushed_down"]

            p50b, p50p = statistics.median(base_lat), statistics.median(push_lat)
            p99b, p99p = pctl(base_lat, 0.99), pctl(push_lat, 0.99)
            p50_reduction = 100 * (1 - p50p / p50b) if p50b else 0.0
            p99_reduction = 100 * (1 - p99p / p99b) if p99b else 0.0

            print(f"{i:>3} {cardinalities[i]:>6} {compress_ratio:>11.2f} {p50b:>13.3f} {p50p:>13.3f} "
                  f"{p99b:>13.3f} {p99p:>13.3f} {p50_reduction:>13.1f}% {p99_reduction:>13.1f}% {str(last_resp_pushed):>10}")

    except urllib.error.HTTPError as e:
        print(f"HTTP error: {e.code} {e.read().decode(errors='replace')}", file=sys.stderr)
        sys.exit(1)
