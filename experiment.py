"""
Fragmentation/read-amplification metric + the one-factor-at-a-time sweep
(granularity is the swept "knob"; k, m and the selectivity regime are held
fixed per curve, matching the plan's "hold everything static, change one
parameter per run" requirement). Multiple trials per point for real error
bars, not a single lucky draw.

Query demand is always defined at the TRUE finest granularity (column
chunk for Parquet, single layer for Neon) and mapped through to whichever
coarser storage unit it landed in -- comparing granularities on the same
logical query, not silently redefining "the query" by how many storage
units happen to exist at each granularity.
"""

import numpy as np
import fac_core


def bin_index(stripes):
    """storage_unit_id -> (stripe_idx, bin_idx, bin_physical_size)"""
    idx = {}
    for si, s in enumerate(stripes):
        for bi, b in enumerate(s.bins):
            bin_size = s.capacity  # physical block size (padded)
            for u in b:
                idx[u.unit_id] = (si, bi, bin_size)
    return idx


def read_cost(stripes, finest_units, finest_to_storage, n_queries, selectivity, rng):
    """finest_units: list[Unit] at true finest granularity (query demand
    basis, fixed across granularities). finest_to_storage: dict mapping
    each finest unit_id -> the storage unit_id it landed inside at the
    granularity under test. Returns two metrics that pull in *opposite*
    directions as granularity varies -- this is where the real tradeoff
    lives, not in either one alone:
      - byte read amplification: fetch = whole bin(s) housing touched
        units vs. bytes actually needed. Finer granularity -> less
        wasted padding per bin -> lower byte amplification.
      - round trips: number of *distinct* bins touched per query --
        Fusion's own actual latency argument (network RTT dominates,
        not raw bytes). Finer granularity -> more, smaller bins exist
        -> a query can end up touching *more* distinct bins, not fewer.
    """
    idx = bin_index(stripes)
    fine_ids = [u.unit_id for u in finest_units]
    fine_size = {u.unit_id: u.size for u in finest_units}

    byte_amp, round_trips = [], []
    for _ in range(n_queries):
        k_sel = max(1, int(round(len(fine_ids) * selectivity)))
        chosen = rng.choice(fine_ids, size=k_sel, replace=False)
        touched_bins = {}
        needed_bytes = 0
        for fid in chosen:
            storage_id = finest_to_storage[fid]
            si, bi, bin_size = idx[storage_id]
            touched_bins[(si, bi)] = bin_size
            needed_bytes += fine_size[fid]
        fetched_bytes = sum(touched_bins.values())
        byte_amp.append(100.0 * (fetched_bytes / needed_bytes - 1.0))
        round_trips.append(len(touched_bins))
    return float(np.mean(byte_amp)), float(np.mean(round_trips))


def run_point(build_storage_fn, build_finest_fn, build_map_fn, k, m, selectivity, n_queries, seed):
    """One trial: build storage-granularity units -> pack -> compute
    overhead; separately build finest-granularity query-demand units +
    their mapping into storage units -> compute read amplification."""
    storage_units = build_storage_fn(seed)
    total_bytes = sum(u.size for u in storage_units)
    stripes = fac_core.construct_stripes(k, storage_units)
    overhead = fac_core.storage_overhead_pct(stripes, k, m, total_bytes)

    finest_units = build_finest_fn(seed)
    finest_to_storage = build_map_fn()

    rng = np.random.default_rng(seed + 1_000_000)
    byte_amp, round_trips = read_cost(stripes, finest_units, finest_to_storage, n_queries, selectivity, rng)
    return overhead, byte_amp, round_trips


def sweep_granularity(build_fns_per_granularity: dict, k: int, m: int,
                       selectivity: float, n_trials: int, n_queries: int,
                       base_seed: int = 0):
    """build_fns_per_granularity: dict[label] -> (build_storage_fn,
    build_finest_fn, build_map_fn) triple. One-factor-at-a-time: only
    granularity varies across the returned curve."""
    results = {}
    for label, (build_storage_fn, build_finest_fn, build_map_fn) in build_fns_per_granularity.items():
        overheads, byte_amps, round_trips = [], [], []
        for t in range(n_trials):
            seed = base_seed + t
            o, ba, rt = run_point(build_storage_fn, build_finest_fn, build_map_fn, k, m, selectivity, n_queries, seed)
            overheads.append(o)
            byte_amps.append(ba)
            round_trips.append(rt)
        overheads = np.array(overheads)
        byte_amps = np.array(byte_amps)
        round_trips = np.array(round_trips)

        def stats(arr):
            return dict(med=float(np.median(arr)), lo=float(np.percentile(arr, 25)), hi=float(np.percentile(arr, 75)))

        results[label] = dict(
            overhead=stats(overheads),
            byte_amp=stats(byte_amps),
            round_trips=stats(round_trips),
            n_trials=n_trials,
        )
    return results
