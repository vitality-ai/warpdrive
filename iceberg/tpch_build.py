#!/usr/bin/env python3
"""Load TPC-H lineitem into two Iceberg tables (plain + column-placed) for a
real-benchmark A/B. Source parquet from `tpchgen-cli parquet -s <SF> --tables
lineitem`. Run: venv/bin/python tpch_build.py
"""
import os, io, time, pyarrow.parquet as pq
from pyiceberg.catalog.rest import RestCatalog
from eval_pushdown_iceberg import column_units, place_file

SRC = os.environ.get("LINEITEM", "/tmp/claude-0/-home-user/1092b597-fec1-566d-bf89-445018d387d4/scratchpad/tpch/lineitem.parquet")
REST = os.environ.get("REST", "http://127.0.0.1:8181")

tbl = pq.read_table(SRC)
print(f"source lineitem: {tbl.num_rows} rows, {tbl.num_columns} cols")
cat = RestCatalog("wd", uri=REST)
cat.create_namespace_if_not_exists("tpch")

for name in ("lineitem_plain", "lineitem_placed"):
    try: cat.drop_table(f"tpch.{name}")
    except Exception: pass
    t = cat.create_table(f"tpch.{name}", schema=tbl.schema)
    t0 = time.time(); t.append(tbl); print(f"  built tpch.{name} in {time.time()-t0:.1f}s")

tc = cat.load_table("tpch.lineitem_placed")
files = [ft.file.file_path for ft in tc.scan().plan_files()]
placements = [place_file(f) for f in files]
print(f"placed {len(files)} data file(s): {placements}")
if not all(p == "packed" for p in placements):
    print("WARNING: some files fell back to plain (overhead/cost gate) -- adjust budget")
else:
    print("all lineitem data files column-placed (content-dependent)")
