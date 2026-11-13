# WarpDial / HiPC 2026 — reproducibility artifacts

This branch (`hipc2026-artifacts`) holds every script, log, and plot behind
`warpdial_hipc2026.tex`'s numbers, so the paper can be reproduced from a
clone plus a running WarpDrive cluster.

## Cluster used for the numbers in the paper

- `warpdrive-distributed`, branch `fix-stripepacker-output-validation`,
  commit `12589d778ab2f1a42f38a6817bc7b486808f709d`.
- 9 nodes, one process each, RS(6,3), launched via
  `warpdrive-distributed/scripts/run_local_cluster.sh` on ports 9710-9718.
- One machine: Apple M3, 8 cores, 16 GB RAM.

This is the same codebase the paper's Methodology section describes. It
already includes the fix for the baseline bug described below.

## The stale-number bug this round corrected

The paper's originally submitted abstract/conclusion numbers (TPC-H Q6
~70x, Lance row-batch ~20x) were measured before commit
`2951b45`/`302af55` ("Skip full EC decode on plain-bucket Range-GETs when
data shards are healthy", 2026-10-07) fixed a real bug in the plain-bucket
baseline: every range-GET decoded the full object before slicing out the
requested range, even when the healthy data shards alone covered it.
After that fix, the plain-bucket baseline got ~8-9x faster, which is why
the speedup *ratio* against the workload-aware path dropped, not because
workload-aware placement regressed.

Verified directly: checking out the pre-fix commit `ed1f729` onto its own
branch (`repro-pre-ec-decode-fix` in `warpdrive-distributed`) and
re-running the same benchmarks there reproduces the old ~70-80x numbers
almost exactly. Re-running on current `main`/`fix-stripepacker-output-validation`
reproduces the new, lower numbers in this directory.

## Numbers reported in the camera-ready paper

From multi-rep runs against the cluster above (see
`run_tpch_reps.py`/`run_lance_reps.py`/`run_sift_reps.py` for the exact
driver code):

| Workload | plain | packed | speedup | n |
|---|---|---|---|---|
| TPC-H Q6 (selective) | 723.4ms (sd 19.5) | 63.0ms (sd 2.7) | 11.5x | 12 |
| TPC-H Q1 (low-sel.) | 3318.8ms | 530.1ms (sd 7.6) | 6.3x | 12 (Q1 plain: 3 direct runs) |
| Lance row-batch nprobe=4 | 54.2ms (sd 1.6) | 18.8ms (sd 1.3) | 2.9x | 12 |
| Lance row-batch nprobe=16 | 53.5ms (sd 1.4) | 28.8ms (sd 2.4) | 1.9x | 12 |
| Lance row-batch nprobe=40 | 53.9ms (sd 2.4) | 26.2ms (sd 4.4) | 2.1x | 12 |
| SIFT1M-small nprobe=5 | 1.77ms (sd 0.07) | 1.47ms (sd 0.14) | 1.2x | 10 |
| SIFT1M-small nprobe=10 | 1.49ms (sd 0.04) | 1.28ms (sd 0.03) | 1.2x | 10 |
| SIFT1M-small nprobe=20 | 1.66ms (sd 0.03) | 1.43ms (sd 0.02) | 1.2x | 10 |

The raw per-rep JSON from that multi-rep session was not preserved in
this session's scratchpad (written outside the project directory, since
cleared). `repro-2026-11-14/*_single_run.log` in this directory are
fresh single-run captures against the same cluster/commit, taken when
this artifacts branch was cut — they land within noise of the table
above (e.g. TPC-H Q6 754ms/70ms vs. the table's 723.4ms/63.0ms mean),
confirming the mean/stdev numbers still reproduce; they are evidence of
reproducibility, not a replacement for a fresh multi-rep run.

Table I's stripes-touched/bytes-reconstructed/overhead numbers are a
geometric property of the packing (not timing), unaffected by the
EC-decode fix. They were re-verified live against this same cluster on
2026-11-14 — see the `server-side evidence` lines in
`repro-2026-11-14/*_single_run.log`.

## How to reproduce from scratch

```
# 1. Build and launch a 9-node cluster from the commit above
cd warpdrive-distributed
git checkout fix-stripepacker-output-validation
cargo build --release
./scripts/run_local_cluster.sh   # RS(6,3), 9 nodes, ports 9710-9718

# 2. From this directory (hipc_poster), with the venv active
python3 -m venv .venv && source .venv/bin/activate
pip install duckdb pylance pyarrow numpy boto3

# 3. Single-run sanity check (what *_single_run.log here came from)
python3 tpch_duckdb_benchmark.py http://127.0.0.1:9710 1.0 \
  warpdrive-distributed/server/cluster-local/node0/logs/application.log
python3 lance_demo.py http://127.0.0.1:9710
python3 sift_benchmark.py http://127.0.0.1:9710   # needs data/siftsmall_*.{fvecs,ivecs}

# 4. Multi-rep mean/stdev (what the paper's table cites)
python3 run_tpch_reps.py http://127.0.0.1:9710 12 /tmp/tpch_reps.json
python3 run_lance_reps.py http://127.0.0.1:9710 12 /tmp/lance_reps.json
python3 run_sift_reps.py http://127.0.0.1:9710 10 /tmp/sift_reps.json

# 5. Regenerate the figure
python3 make_hipc_plots.py
cp images/all_results_rs63.png plots/all_results_rs63.png

# 6. Recompile the paper
tectonic warpdial_hipc2026.tex
```

`data/siftsmall_*.{fvecs,ivecs}` are committed directly in this branch
(downloaded from the TileDB-Inc mirror of the texmex SIFT1M-small
corpus) so step 3/6 above has no external dependency.
`data/lineitem_sf1.0_sorted.parquet` is **not** committed (199MB,
regenerated automatically by `tpch_duckdb_benchmark.py` via DuckDB's own
`dbgen` on first run).
