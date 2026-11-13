"""
Entry point. Two phases:
  1. Fusion-reproduction sanity check (Parquet, column_chunk, RS(9,6)) --
     must land in the same order of magnitude as Fusion's own reported
     ~1.2% overhead, or the simulator has a bug and nothing else can be
     trusted.
  2. The real sweep: granularity is the one swept knob (one-factor-at-a-
     time), k/m and selectivity held fixed per curve; repeated across 3
     selectivity regimes as a robustness check, N_TRIALS fresh-seed runs
     per point for real error bars.
"""
import json
import sys
import time
from functools import partial

import numpy as np

import fac_core
import formats
from experiment import sweep_granularity

# ---- fixed EC params for this whole run (Fusion's own default) ----
K, M = 9, 6

# ---- rigor knobs ----
N_TRIALS = 25
N_QUERIES_PER_TRIAL = 300
SELECTIVITY_REGIMES = {"low_selectivity_1pct": 0.01, "med_selectivity_10pct": 0.10, "high_selectivity_50pct": 0.50}

# ---- dataset sizing (kept modest for turnaround; shape matches Fusion's
# own TPC-H lineitem eval: 16 cols. Real run bumps N_ROWS/N_VECTORS up.) ----
PARQUET_N_ROWS = int(sys.argv[1]) if len(sys.argv) > 1 else 300_000
PARQUET_N_ROW_GROUPS = 10
VECTOR_N_VECTORS = int(sys.argv[2]) if len(sys.argv) > 2 else 20_000
VECTOR_EMBED_DIM = formats.EMBED_DIM
VECTOR_NLIST = formats.vector_nlist(VECTOR_N_VECTORS)  # FAISS's own 4*sqrt(N) heuristic


def log(msg):
    print(f"[{time.strftime('%H:%M:%S')}] {msg}", flush=True)


def fusion_reproduction_check():
    log("=== Fusion-reproduction sanity check (Parquet, column_chunk, RS(9,6)) ===")
    pbytes = formats.make_synthetic_tpch_lineitem_like(PARQUET_N_ROWS, PARQUET_N_ROW_GROUPS, seed=0)
    units = formats.parquet_units(pbytes, "column_chunk")
    total = sum(u.size for u in units)
    log(f"  {len(units)} column-chunk units, {total/1e6:.1f} MB total compressed")
    stripes = fac_core.construct_stripes(K, units)
    overhead = fac_core.storage_overhead_pct(stripes, K, M, total)
    log(f"  FAC storage overhead vs optimal: {overhead:.3f}%  (Fusion's own paper reports ~1.2% on their real 10GB lineitem file)")
    return overhead


N_PARQUET_COLUMNS = 16  # matches make_synthetic_tpch_lineitem_like's column count


def parquet_triple(batch_size, n_rows, n_row_groups):
    def build_storage(seed):
        pbytes = formats.make_synthetic_tpch_lineitem_like(n_rows, n_row_groups, seed=seed)
        return formats.parquet_units_batched(pbytes, batch_size)

    def build_finest(seed):
        pbytes = formats.make_synthetic_tpch_lineitem_like(n_rows, n_row_groups, seed=seed)
        return formats.parquet_units(pbytes, "column_chunk")

    def build_map():
        return formats.parquet_finest_to_storage_map_batched(n_row_groups, N_PARQUET_COLUMNS, batch_size)

    return build_storage, build_finest, build_map


def vector_triple(granularity, n_vectors, embed_dim, nlist):
    def build_storage(seed):
        return formats.vector_partition_units(n_vectors, embed_dim, nlist, granularity, seed=seed)

    def build_finest(seed):
        return formats.vector_finest_partitions(n_vectors, embed_dim, nlist, seed=seed)

    def build_map():
        return formats.vector_finest_to_storage_map(nlist, granularity)

    return build_storage, build_finest, build_map


def main():
    t0 = time.time()
    repro = fusion_reproduction_check()

    all_results = {"fusion_reproduction_overhead_pct": repro, "k": K, "m": M,
                   "n_trials": N_TRIALS, "n_queries_per_trial": N_QUERIES_PER_TRIAL,
                   "parquet_n_rows": PARQUET_N_ROWS, "vector_n_vectors": VECTOR_N_VECTORS,
                   "vector_embed_dim": VECTOR_EMBED_DIM, "vector_nlist": VECTOR_NLIST,
                   "parquet": {}, "vector": {}}

    # Dense batch-size sweeps (same mechanism, both formats): batch_size=1 is
    # the finest (never split a computable unit), the largest batch_size
    # collapses to a single whole-object unit (coarsest). Every value in
    # between is a real, distinct point on the frontier, not a hand-picked
    # label -- this is what makes it an actual tradeoff curve rather than
    # a 3-point sketch.
    PARQUET_TOTAL_UNITS = PARQUET_N_ROW_GROUPS * N_PARQUET_COLUMNS
    parquet_batch_sizes = sorted(set(
        b for b in [1, 2, 4, 8, 16, 32, 80, PARQUET_TOTAL_UNITS] if b <= PARQUET_TOTAL_UNITS
    ))
    vector_batch_sizes = sorted(set(
        b for b in [1, 2, 4, 8, 16, 32, 64, 128, VECTOR_NLIST] if b <= VECTOR_NLIST
    ))

    def label(b, total):
        tag = " (finest)" if b == 1 else " (coarsest, whole index)" if b == total else ""
        return f"batch_{b}{tag}"

    parquet_granularities = {
        label(b, PARQUET_TOTAL_UNITS): parquet_triple(b, PARQUET_N_ROWS, PARQUET_N_ROW_GROUPS)
        for b in parquet_batch_sizes
    }
    vector_granularities = {
        label(b, VECTOR_NLIST): vector_triple(b, VECTOR_N_VECTORS, VECTOR_EMBED_DIM, VECTOR_NLIST)
        for b in vector_batch_sizes
    }

    for regime_name, sel in SELECTIVITY_REGIMES.items():
        log(f"=== Parquet sweep, selectivity regime: {regime_name} ({sel}) ===")
        res = sweep_granularity(parquet_granularities, K, M, sel, N_TRIALS, N_QUERIES_PER_TRIAL, base_seed=1)
        all_results["parquet"][regime_name] = res
        for gran_label, r in res.items():
            log(f"    {gran_label:45s} overhead={r['overhead']['med']:.3f}%  "
                f"byte_amp={r['byte_amp']['med']:.1f}%  round_trips={r['round_trips']['med']:.2f}")

        log(f"=== Vector-partition sweep, selectivity regime: {regime_name} ({sel}) ===")
        res = sweep_granularity(vector_granularities, K, M, sel, N_TRIALS, N_QUERIES_PER_TRIAL, base_seed=1)
        all_results["vector"][regime_name] = res
        for gran_label, r in res.items():
            log(f"    {gran_label:45s} overhead={r['overhead']['med']:.3f}%  "
                f"byte_amp={r['byte_amp']['med']:.1f}%  round_trips={r['round_trips']['med']:.2f}")

    with open("results.json", "w") as f:
        json.dump(all_results, f, indent=2)
    log(f"Wrote results.json. Total runtime: {time.time()-t0:.1f}s")


if __name__ == "__main__":
    main()
