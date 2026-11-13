"""
Stores a REAL Parquet file in WarpDrive (not real_workload_driver.py's
synthetic size-matched filler), with real file-offset computable units
(parquet_real_offsets.py), so an unmodified third-party tool can read it
back. Step 1: PUT + verify both byte-identical round-trip AND that pyarrow
can actually re-open the result as a valid Parquet file. DuckDB querying
(via Range-GET, once the Rust side supports it) is a separate, later step
in this same script.

Usage: python3 duckdb_demo.py <cluster_base_url>
"""
import io
import json
import sys
import urllib.request

import pyarrow.parquet as pq

from formats import make_synthetic_tpch_lineitem_like
from parquet_real_offsets import parquet_real_units, verify_full_coverage
from real_workload_driver import configure_bucket_for_content_dependent_placement, http_get, http_put


def put_real_parquet(cluster_url: str, bucket: str, key: str, parquet_bytes: bytes, batch_size: int) -> dict:
    units = parquet_real_units(parquet_bytes, batch_size=batch_size)
    verify_full_coverage(units, len(parquet_bytes))

    header = json.dumps([[offset, length] for _unit_id, offset, length in units])
    http_put(
        f"{cluster_url}/cluster/{bucket}/{key}",
        parquet_bytes,
        {"x-warpd-computable-units": header, "Content-Type": "application/octet-stream"},
    )

    got = http_get(f"{cluster_url}/cluster/{bucket}/{key}")
    byte_identical = got == parquet_bytes

    # The real bar: can an unmodified third-party Parquet reader open what
    # came back? Byte-identical implies this, but checking it directly
    # catches a coverage bug that happened to produce the same length.
    pyarrow_readable = False
    row_count = None
    try:
        table = pq.read_table(io.BytesIO(got))
        pyarrow_readable = True
        row_count = table.num_rows
    except Exception as e:
        pyarrow_error = str(e)
    else:
        pyarrow_error = None

    record_bytes = http_get(f"{cluster_url}/cluster/_internal/content_record/{bucket}/{key}")
    record = json.loads(record_bytes)

    return {
        "num_units": len(units),
        "num_stripes": len(record["stripes"]),
        "file_bytes": len(parquet_bytes),
        "byte_identical_round_trip": byte_identical,
        "pyarrow_readable": pyarrow_readable,
        "pyarrow_row_count": row_count,
        "pyarrow_error": pyarrow_error,
    }


if __name__ == "__main__":
    cluster_url = sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:9710"

    pbytes = make_synthetic_tpch_lineitem_like(5_000, 10, seed=42)

    configure_bucket_for_content_dependent_placement(cluster_url, "duckdbdemo", overhead_threshold_pct=100.0)

    print("=== Real Parquet file, column-chunk granularity (batch_size=1) ===")
    print(json.dumps(put_real_parquet(cluster_url, "duckdbdemo", "lineitem-colchunk", pbytes, batch_size=1), indent=2))

    print("=== Real Parquet file, row-group granularity (batch_size=16) ===")
    print(json.dumps(put_real_parquet(cluster_url, "duckdbdemo", "lineitem-rowgroup", pbytes, batch_size=16), indent=2))
