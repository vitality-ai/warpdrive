#!/usr/bin/env python3
"""Load the full TPC-H schema (SF=1) into Iceberg as <table>_plain and
<table>_placed, then column-place the _placed data files. Reports which tables
actually placed vs fell back to plain via the cost gate (tiny tables should fall
back -- their fan-out dwarfs their size). Run: venv/bin/python tpch_load_all.py
"""
import os, pyarrow.parquet as pq
from pyiceberg.catalog.rest import RestCatalog
from eval_pushdown_iceberg import column_units, place_file

SRC = os.environ.get("TPCH_DIR", "/tmp/claude-0/-home-user/1092b597-fec1-566d-bf89-445018d387d4/scratchpad/tpch")
TABLES = ["lineitem", "orders", "customer", "part", "partsupp", "supplier", "nation", "region"]
cat = RestCatalog("wd", uri=os.environ.get("REST", "http://127.0.0.1:8181"))
cat.create_namespace_if_not_exists("tpch")

for name in TABLES:
    tbl = pq.read_table(f"{SRC}/{name}.parquet")
    for v in ("plain", "placed"):
        try: cat.drop_table(f"tpch.{name}_{v}")
        except Exception: pass
        t = cat.create_table(f"tpch.{name}_{v}", schema=tbl.schema)
        t.append(tbl)
    tc = cat.load_table(f"tpch.{name}_placed")
    files = [ft.file.file_path for ft in tc.scan().plan_files()]
    placements = [place_file(f) for f in files]
    tag = "PLACED" if files and all(p == "packed" for p in placements) else \
          ("mixed" if "packed" in placements else "plain(gate declined)")
    print(f"  {name:10s} {tbl.num_rows:>9} rows  {len(files)} file(s)  -> {tag} {placements}")
print("loaded full TPC-H schema (plain + placed)")
