#!/usr/bin/env python3
"""Build two identical Iceberg tables for the wall-clock benchmark:
bench.plain (plain EC) and bench.placed (column-grouped content-dependent).
Wider schema so a 1-column projection is a small fraction of the file.
Run: venv/bin/python bench_build.py
"""
import os, pyarrow as pa
from pyiceberg.catalog.rest import RestCatalog
from eval_pushdown_iceberg import column_units, place_file  # reuse placement

REST = os.environ.get("REST", "http://127.0.0.1:8181")
NCOL = int(os.environ.get("NCOL", "32"))
NROW = int(os.environ.get("NROW", "400000"))

cat = RestCatalog("wd", uri=REST)
cat.create_namespace_if_not_exists("bench")
data = pa.table({f"c{i}": pa.array([(i * 1_000_000 + j) if i % 2 else float(j) * 1.5 for j in range(NROW)])
                 for i in range(NCOL)})

for name in ("plain", "placed"):
    try: cat.drop_table(f"bench.{name}")
    except Exception: pass
    t = cat.create_table(f"bench.{name}", schema=data.schema)
    t.append(data)

tc = cat.load_table("bench.placed")
files = [ft.file.file_path for ft in tc.scan().plan_files()]
placements = [place_file(f) for f in files]
print(f"NCOL={NCOL} NROW={NROW}; placed {len(files)} data file(s): {set(placements)}")
assert placements and all(p == "packed" for p in placements), "placement did not engage"
print("built bench.plain (plain) and bench.placed (column-grouped)")
