"""
Generic file-format-aware-coding (FAC) core, generalized from Fusion's
Algorithm 1 (Lu, Raina, Cidon, Freedman -- ASPLOS'25).

Takes a flat list of (unit_id, size_bytes) computable units and a stripe
width k. No format-specific logic lives here -- Parquet column chunks and
Neon layer groups are just different (unit_id, size) lists fed into the
same function.
"""

from dataclasses import dataclass, field


@dataclass
class Unit:
    unit_id: str
    size: int


@dataclass
class Stripe:
    bins: list  # list[list[Unit]], length k
    capacity: int  # C: size of the largest (seed) bin == parity block size


def construct_stripes(k: int, units: list[Unit]) -> list[Stripe]:
    """Fusion Algorithm 1, generalized. O(m*N) where m = number of stripes,
    N = number of units. Never splits a unit across stripes."""
    remaining = sorted(units, key=lambda u: -u.size)
    stripes: list[Stripe] = []

    while remaining:
        seed = remaining.pop(0)
        bins = [[] for _ in range(k)]
        bins[0].append(seed)
        bin_sizes = [seed.size] + [0] * (k - 1)
        capacity = seed.size

        still_remaining = []
        for u in remaining:
            # least-occupied bin (excluding bin 0) that still fits u
            candidates = [
                (bin_sizes[b], b) for b in range(1, k) if bin_sizes[b] + u.size <= capacity
            ]
            if candidates:
                _, bid = min(candidates)
                bins[bid].append(u)
                bin_sizes[bid] += u.size
            else:
                still_remaining.append(u)
        remaining = still_remaining

        stripes.append(Stripe(bins=bins, capacity=capacity))

    return stripes


def storage_overhead_pct(stripes: list[Stripe], k: int, m: int, total_original_bytes: int) -> float:
    """Fusion's own metric: % additional physical bytes vs the optimal
    (1 + m/k) * total_original_bytes achievable with zero padding."""
    actual_physical = sum((k + m) * s.capacity for s in stripes)
    optimal_physical = (1 + m / k) * total_original_bytes
    return 100.0 * (actual_physical / optimal_physical - 1.0)


def unit_to_stripe_index(stripes: list[Stripe]) -> dict[str, int]:
    idx = {}
    for i, s in enumerate(stripes):
        for bin_ in s.bins:
            for u in bin_:
                idx[u.unit_id] = i
    return idx
