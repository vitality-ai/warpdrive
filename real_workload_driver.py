"""
Drives real traffic against the live WarpDrive distributed cluster using
the SAME workload-shape generators as the Python simulator (formats.py),
replacing the simulator's cost-model computation with real measured
numbers (docs/Distributed-Engine-Plan.md, phase 3).

Body bytes are synthetic filler sized to match each unit's real size --
exactly like the simulator itself, which only ever worked with
(unit_id, size) pairs via fac_core.Unit, never actual file content.
What's "real" here is that these sizes now get bin-packed, erasure-coded,
and stored by the live Rust cluster (FacPacker, a faithful port of this
same fac_core.py), with the storage-overhead number read back from what
the real packer actually decided (via the cluster_content_record
diagnostic endpoint), not recomputed separately in Python.

Usage: python3 real_workload_driver.py <cluster_base_url>
Example: python3 real_workload_driver.py http://127.0.0.1:9710
"""
import json
import sys
import urllib.error
import urllib.request

from fac_core import Unit
from formats import (
    EMBED_DIM,
    make_synthetic_tpch_lineitem_like,
    parquet_units_batched,
    vector_nlist,
    vector_partition_units,
)


def synthetic_body(units: list[Unit]) -> bytes:
    """Filler bytes matching each unit's real size, concatenated in order --
    content doesn't matter to WarpDrive or to the storage-overhead metric,
    only sizes do (see module docstring)."""
    return b"".join(bytes([i % 251]) * u.size for i, u in enumerate(units))


def units_header(units: list[Unit]) -> str:
    offsets = []
    pos = 0
    for u in units:
        offsets.append([pos, u.size])
        pos += u.size
    return json.dumps(offsets)


def http_put(url: str, body: bytes, headers: dict) -> None:
    req = urllib.request.Request(url, data=body, headers=headers, method="PUT")
    with urllib.request.urlopen(req, timeout=60) as resp:
        if resp.status != 200:
            raise RuntimeError(f"PUT {url} returned {resp.status}")


def configure_bucket_for_content_dependent_placement(
    cluster_url: str, bucket: str, overhead_threshold_pct: float, packer_name: str = "fac"
) -> None:
    """Content-dependent placement is a per-bucket opt-in now (bucket_config.rs) --
    a bucket with no config always takes the plain path, regardless of any
    header. Must be called once before PUTting to a bucket that should use
    content-dependent packing. `packer_name` selects which registered
    packer the bucket uses -- "fac" (Parquet/size-based) or "ivf_centroid"
    (spatial, see lance_demo.py), or any future registered packer."""
    req = urllib.request.Request(
        f"{cluster_url}/cluster/_admin/bucket_config/{bucket}",
        data=json.dumps({"packer_name": packer_name, "overhead_threshold_pct": overhead_threshold_pct}).encode(),
        headers={"Content-Type": "application/json"},
        method="PUT",
    )
    urllib.request.urlopen(req, timeout=30).read()


def http_get(url: str) -> bytes:
    with urllib.request.urlopen(url, timeout=60) as resp:
        return resp.read()


def put_and_measure(cluster_url: str, bucket: str, key: str, units: list[Unit]) -> dict:
    body = synthetic_body(units)
    header = units_header(units)

    http_put(
        f"{cluster_url}/cluster/{bucket}/{key}",
        body,
        {"x-warpd-computable-units": header, "Content-Type": "application/octet-stream"},
    )

    got = http_get(f"{cluster_url}/cluster/{bucket}/{key}")
    if got != body:
        raise AssertionError("round-trip mismatch -- real cluster did not reconstruct correctly")

    record_bytes = http_get(f"{cluster_url}/cluster/_internal/content_record/{bucket}/{key}")
    record = json.loads(record_bytes)

    k, m = record["k"], record["m"]
    total_original = sum(u.size for u in units)
    actual_physical = sum((k + m) * s["capacity"] for s in record["stripes"])
    optimal_physical = (1 + m / k) * total_original
    overhead_pct = 100.0 * (actual_physical / optimal_physical - 1.0)

    return {
        "num_stripes": len(record["stripes"]),
        "total_original_bytes": total_original,
        "actual_physical_bytes": actual_physical,
        "storage_overhead_pct": round(overhead_pct, 4),
        "round_trip_verified": True,
    }


def run_parquet(cluster_url: str, batch_size: int, n_rows: int, n_row_groups: int, seed: int = 42) -> dict:
    pq_bytes = make_synthetic_tpch_lineitem_like(n_rows, n_row_groups, seed)
    units = parquet_units_batched(pq_bytes, batch_size)
    result = put_and_measure(cluster_url, "realfac", f"parquet-batch{batch_size}", units)
    result.update(workload="parquet", batch_size=batch_size, num_units=len(units))
    return result


def run_vector(cluster_url: str, granularity: int, n_vectors: int, seed: int = 42) -> dict:
    nlist = vector_nlist(n_vectors)
    units = vector_partition_units(n_vectors, EMBED_DIM, nlist, granularity, seed)
    result = put_and_measure(cluster_url, "realfac", f"vector-gran{granularity}", units)
    result.update(workload="vector", granularity=granularity, num_units=len(units), nlist=nlist)
    return result


if __name__ == "__main__":
    cluster_url = sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:9710"
    scale = sys.argv[2] if len(sys.argv) > 2 else "small"
    if scale == "full":
        # The poster's/simulator's own reproduction parameters (run_all.py's
        # defaults: PARQUET_N_ROWS=300_000, PARQUET_N_ROW_GROUPS=10,
        # VECTOR_N_VECTORS=20_000), run against RS(9,6) -- Fusion's paper's
        # own default erasure code -- for numbers directly comparable to
        # the poster's reported figures, not just a mechanism check.
        n_rows, n_row_groups, n_vectors = 300_000, 10, 20_000
    else:
        # Small sizes, for a fast mechanism-correctness pass.
        n_rows, n_row_groups, n_vectors = 5_000, 10, 5_000

    try:
        # Permissive threshold: this script's whole point is to observe the
        # real overhead across granularities, including the deliberately
        # bad ones (row-group batching), so content-dependent placement
        # must actually be attempted for every case, not silently fall
        # back once overhead gets high. A production bucket would use a
        # much tighter threshold (see pushdown_benchmark.py / bucket_config.rs).
        configure_bucket_for_content_dependent_placement(cluster_url, "realfac", overhead_threshold_pct=100.0)

        print("=== Parquet, column-chunk granularity (batch_size=1) ===")
        print(json.dumps(run_parquet(cluster_url, 1, n_rows, n_row_groups), indent=2))

        print("=== Parquet, row-group granularity (batch_size=16) ===")
        print(json.dumps(run_parquet(cluster_url, 16, n_rows, n_row_groups), indent=2))

        print("=== Vector index, finest granularity (granularity=1) ===")
        print(json.dumps(run_vector(cluster_url, 1, n_vectors), indent=2))

        print("=== Vector index, batch_size=16 ===")
        print(json.dumps(run_vector(cluster_url, 16, n_vectors), indent=2))
    except urllib.error.HTTPError as e:
        print(f"HTTP error: {e.code} {e.read().decode(errors='replace')}", file=sys.stderr)
        sys.exit(1)
