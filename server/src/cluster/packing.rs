//! The contract content-dependent placement needs from a bin-packing
//! strategy — not a specific algorithm. `FacPacker` (below) implements it
//! using Fusion's Algorithm 1, generalized (Lu, Raina, Cidon, Freedman,
//! ASPLOS'25), ported faithfully from this project's own `fac_core.py`.
//! Anyone could implement `StripePacker` with a different strategy without
//! touching `ContentDependentPlacement`, the EC layer, or `coordinator.rs`
//! at all — the same reason every other component here (`Storage`,
//! `PlacementPolicy`, `LocationStore`, `ErasureCoder`, `PeerClient`) is a
//! trait with one concrete implementation, not a concrete type.

use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct Unit {
    pub unit_id: String,
    pub size: usize,
    /// Opaque, packer-defined bytes — `FacPacker` ignores this entirely
    /// (it only ever looks at `size`), but a different packer can use it
    /// for whatever its own algorithm needs: e.g. a workload-aware packer
    /// for IVF vector partitions reads this as an encoded spatial-locality
    /// rank derived from the partition's real centroid, so it can group
    /// partitions that tend to be probed *together* into the same stripe —
    /// something size alone can never express. Empty for every caller that
    /// doesn't need it (the default, zero behavior change for FacPacker).
    pub metadata: Vec<u8>,
}

/// `k` bins (each a list of unit_ids, in the order they were packed into
/// that bin), all sized at most `capacity` — the shared pad-to size every
/// bin in this stripe needs for `ErasureCoder::encode_shards`.
#[derive(Debug, Clone)]
pub struct Stripe {
    pub bins: Vec<Vec<String>>,
    pub capacity: usize,
}

pub trait StripePacker: Send + Sync {
    /// Partition `units` into stripes of `k` bins each. Never splits a
    /// unit across stripes or bins — the whole point of a computable unit
    /// is that it survives intact.
    fn pack(&self, k: usize, units: &[Unit]) -> Vec<Stripe>;
}

/// Real storage overhead of a packing result, w.r.t. the optimal MDS size
/// `(1 + m/k) * total_original_bytes` — Fusion's own definition (ASPLOS'25
/// §6.3). This is the number the per-bucket overhead threshold (see
/// `bucket_config.rs`) is compared against: computed once, before any shard
/// is written, so an over-threshold pack can be abandoned with zero network
/// cost, not discovered after the fact.
pub fn overhead_pct(k: usize, m: usize, units: &[Unit], stripes: &[Stripe]) -> f64 {
    let total_original: usize = units.iter().map(|u| u.size).sum();
    if total_original == 0 {
        return 0.0;
    }
    let actual_physical: usize = stripes.iter().map(|s| (k + m) * s.capacity).sum();
    let optimal_physical = (1.0 + m as f64 / k as f64) * total_original as f64;
    100.0 * (actual_physical as f64 / optimal_physical - 1.0)
}

/// Fusion's Algorithm 1, generalized, as a standalone helper — not just
/// `FacPacker`'s private body. Greedy: seed each stripe with the largest
/// remaining unit, pack the rest into whichever other bin is least full
/// and still has room under the seed's size (the stripe's `capacity`).
/// Extracted so a workload-aware packer (`IvfCentroidPacker` below) can
/// apply this same real bin-packing *within* a group of units it already
/// knows belong together, instead of inventing a cruder one-unit-per-bin
/// rule. Real bin-packing efficiency and workload-awareness aren't
/// alternatives — they compose.
fn fac_pack(k: usize, units: &[&Unit]) -> Vec<Stripe> {
    let mut remaining: Vec<&Unit> = units.to_vec();
    remaining.sort_by(|a, b| b.size.cmp(&a.size));

    let mut stripes = Vec::new();

    while !remaining.is_empty() {
        let seed = remaining.remove(0);
        let mut bins: Vec<Vec<String>> = vec![Vec::new(); k];
        bins[0].push(seed.unit_id.clone());
        let mut bin_sizes = vec![0usize; k];
        bin_sizes[0] = seed.size;
        let capacity = seed.size;

        let mut still_remaining = Vec::new();
        for u in remaining {
            let candidate = (1..k)
                .filter(|&b| bin_sizes[b] + u.size <= capacity)
                .min_by_key(|&b| bin_sizes[b]);
            match candidate {
                Some(b) => {
                    bins[b].push(u.unit_id.clone());
                    bin_sizes[b] += u.size;
                }
                None => still_remaining.push(u),
            }
        }
        remaining = still_remaining;

        stripes.push(Stripe { bins, capacity });
    }

    stripes
}

/// FAC: Fusion's Algorithm 1, generalized — the one implementation shipped
/// here. Thin wrapper over `fac_pack`, applied globally across all units.
pub struct FacPacker;

impl StripePacker for FacPacker {
    fn pack(&self, k: usize, units: &[Unit]) -> Vec<Stripe> {
        let refs: Vec<&Unit> = units.iter().collect();
        fac_pack(k, &refs)
    }
}

/// A second, deliberately *different* `StripePacker` — not a generalization
/// of FAC, a genuinely different algorithm motivated by how IVF vector
/// search actually accesses data. An `nprobe` query touches whichever
/// centroids are geometrically nearest the query vector, and nearby
/// centroids tend to get probed together. FAC only ever looks at
/// `Unit::size` — it has no way to express "these two units are usually
/// needed together."
///
/// This packer reads `Unit::metadata` as a little-endian `u32` *cluster
/// id* — a real k-means cluster assignment over the index's actual
/// centroids (see `hipc_poster/lance_real_offsets.py`), not a guessed or
/// random grouping. Units are grouped by *exact* cluster id (not just
/// sorted-order proximity — an earlier version of this packer sorted by a
/// flattened 1-D "spatial rank" and chunked every `k`, which only
/// guarantees chain-adjacent units are close, not that an arbitrary
/// query's whole probed set is; measured directly, that version showed no
/// improvement over no packing at all). Within each cluster, `fac_pack`
/// runs exactly as `FacPacker` would — real multi-unit-per-bin packing,
/// not one unit per bin — so a cluster of many partitions still fills a
/// stripe's bins efficiently instead of spilling into one stripe per `k`
/// partitions regardless of how many the cluster actually has.
pub struct IvfCentroidPacker;

fn cluster_id(u: &Unit) -> u32 {
    if u.metadata.len() >= 4 {
        u32::from_le_bytes([u.metadata[0], u.metadata[1], u.metadata[2], u.metadata[3]])
    } else {
        u32::MAX // unranked (e.g. framing) units: their own group, sorted last
    }
}

impl StripePacker for IvfCentroidPacker {
    fn pack(&self, k: usize, units: &[Unit]) -> Vec<Stripe> {
        // A HashMap for the grouping lookup (#161): a `.find()` per unit
        // over an ever-growing `groups` Vec is O(units x distinct
        // clusters), which an IVF index with thousands of partitions
        // across even a few dozen clusters starts to feel.
        let mut groups: Vec<(u32, Vec<&Unit>)> = Vec::new();
        let mut index_of: HashMap<u32, usize> = HashMap::new();
        for u in units {
            let cid = cluster_id(u);
            match index_of.get(&cid) {
                Some(&idx) => groups[idx].1.push(u),
                None => {
                    index_of.insert(cid, groups.len());
                    groups.push((cid, vec![u]));
                }
            }
        }
        groups.sort_by_key(|(id, _)| *id);

        groups.into_iter().flat_map(|(_, us)| fac_pack(k, &us)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn units(sizes: &[usize]) -> Vec<Unit> {
        sizes
            .iter()
            .enumerate()
            .map(|(i, &size)| Unit { unit_id: format!("u{i}"), size, metadata: Vec::new() })
            .collect()
    }

    #[test]
    fn never_splits_a_unit_and_every_unit_appears_exactly_once() {
        let u = units(&[100, 80, 60, 40, 20, 10, 5]);
        let stripes = FacPacker.pack(3, &u);

        let mut seen = std::collections::HashSet::new();
        for s in &stripes {
            for bin in &s.bins {
                for id in bin {
                    assert!(seen.insert(id.clone()), "unit {id} appeared more than once");
                }
            }
        }
        assert_eq!(seen.len(), u.len());
    }

    #[test]
    fn no_bin_exceeds_its_stripes_capacity() {
        let u = units(&[100, 80, 60, 45, 45, 45, 10, 10, 10, 10]);
        let stripes = FacPacker.pack(3, &u);
        let by_id: std::collections::HashMap<_, _> =
            u.iter().map(|unit| (unit.unit_id.clone(), unit.size)).collect();

        for s in &stripes {
            for bin in &s.bins {
                let total: usize = bin.iter().map(|id| by_id[id]).sum();
                assert!(total <= s.capacity, "bin total {total} exceeds capacity {}", s.capacity);
            }
        }
    }

    #[test]
    fn matches_fac_core_py_reference_trace() {
        // Same input/k as a hand-traced run of fac_core.py's construct_stripes,
        // to catch any porting mistake (off-by-one in bin selection, wrong
        // seed order, etc.) that unit-count/capacity invariants alone
        // wouldn't necessarily catch.
        let u = units(&[50, 30, 30, 20, 20, 10]);
        let stripes = FacPacker.pack(3, &u);

        // seed (50) -> bin 0; then greedily: 30->bin1 (0+30<=50), 30->bin2 (0+30<=50),
        // 20->bin1 (tie with bin2 at 30 each, first wins: 30+20=50<=50),
        // 20->bin2 (30+20=50<=50), 10-> neither bin1 nor bin2 fits (50+10>50)
        // -> leftover, starts its own second stripe.
        assert_eq!(stripes.len(), 2);

        let s0 = &stripes[0];
        assert_eq!(s0.capacity, 50);
        assert_eq!(s0.bins[0], vec!["u0"]);
        assert_eq!(s0.bins[1], vec!["u1", "u3"]);
        assert_eq!(s0.bins[2], vec!["u2", "u4"]);

        let s1 = &stripes[1];
        assert_eq!(s1.capacity, 10);
        assert_eq!(s1.bins[0], vec!["u5"]);
        assert_eq!(s1.bins[1], Vec::<String>::new());
        assert_eq!(s1.bins[2], Vec::<String>::new());
    }

    #[test]
    fn overhead_pct_matches_a_real_measured_run() {
        // Grounded in an actual local-cluster run (RS(3,2), column-chunk
        // granularity Parquet workload): total_original=256375,
        // actual_physical=429370 -> 0.4864% overhead. Not a hand-derived
        // number — this is what the live FacPacker produced.
        let u = vec![Unit { unit_id: "u".into(), size: 256_375, metadata: Vec::new() }];
        let stripes = vec![Stripe { bins: vec![Vec::new(); 3], capacity: 85_874 }];
        let pct = overhead_pct(3, 2, &u, &stripes);
        assert!((pct - 0.4864).abs() < 0.01, "got {pct}");
    }

    fn units_with_rank(ranks: &[(usize, u32)]) -> Vec<Unit> {
        ranks
            .iter()
            .enumerate()
            .map(|(i, &(size, rank))| Unit {
                unit_id: format!("u{i}"),
                size,
                metadata: rank.to_le_bytes().to_vec(),
            })
            .collect()
    }

    #[test]
    fn ivf_centroid_packer_never_splits_a_unit_and_every_unit_appears_exactly_once() {
        // cluster ids (the second element) deliberately repeat, so this
        // also exercises grouping, not just the single-unit-per-id case.
        let u = units_with_rank(&[(10, 0), (20, 0), (15, 1), (30, 1), (5, 1), (8, 2)]);
        let stripes = IvfCentroidPacker.pack(3, &u);

        let mut seen = std::collections::HashSet::new();
        for s in &stripes {
            for bin in &s.bins {
                for id in bin {
                    assert!(seen.insert(id.clone()), "unit {id} appeared more than once");
                }
            }
        }
        assert_eq!(seen.len(), u.len());
    }

    #[test]
    fn ivf_centroid_packer_no_bin_exceeds_its_stripes_capacity() {
        let u = units_with_rank(&[(10, 0), (20, 0), (15, 1), (30, 1), (5, 1), (8, 2)]);
        let stripes = IvfCentroidPacker.pack(3, &u);
        let by_id: std::collections::HashMap<_, _> = u.iter().map(|unit| (unit.unit_id.clone(), unit.size)).collect();

        for s in &stripes {
            for bin in &s.bins {
                let total: usize = bin.iter().map(|id| by_id[id]).sum();
                assert!(total <= s.capacity, "bin total {total} exceeds capacity {}", s.capacity);
            }
        }
    }

    #[test]
    fn ivf_centroid_packer_groups_by_exact_cluster_id_not_by_size() {
        // near_a/b/c share cluster id 0; far_a/b/c share cluster id 1.
        // Sizes are deliberately scrambled so a size-based packer
        // (FacPacker) would group these completely differently -- this
        // test specifically checks IvfCentroidPacker groups by cluster id,
        // not by size.
        let u = vec![
            Unit { unit_id: "near_a".into(), size: 100, metadata: 0u32.to_le_bytes().to_vec() },
            Unit { unit_id: "near_b".into(), size: 10, metadata: 0u32.to_le_bytes().to_vec() },
            Unit { unit_id: "near_c".into(), size: 50, metadata: 0u32.to_le_bytes().to_vec() },
            Unit { unit_id: "far_a".into(), size: 20, metadata: 1u32.to_le_bytes().to_vec() },
            Unit { unit_id: "far_b".into(), size: 90, metadata: 1u32.to_le_bytes().to_vec() },
            Unit { unit_id: "far_c".into(), size: 5, metadata: 1u32.to_le_bytes().to_vec() },
        ];
        let stripes = IvfCentroidPacker.pack(3, &u);
        assert_eq!(stripes.len(), 2);

        let stripe0_ids: std::collections::HashSet<_> = stripes[0].bins.iter().flatten().cloned().collect();
        assert_eq!(
            stripe0_ids,
            ["near_a", "near_b", "near_c"].iter().map(|s| s.to_string()).collect()
        );
        let stripe1_ids: std::collections::HashSet<_> = stripes[1].bins.iter().flatten().cloned().collect();
        assert_eq!(
            stripe1_ids,
            ["far_a", "far_b", "far_c"].iter().map(|s| s.to_string()).collect()
        );
    }

    #[test]
    fn ivf_centroid_packer_packs_multiple_units_per_bin_within_a_cluster() {
        // A single cluster of 4 small units under k=3 bins should use real
        // bin-packing (multiple units sharing a bin) rather than needing
        // 2 stripes for a one-unit-per-bin rule -- this is the entire
        // point of running fac_pack within each cluster instead of a
        // naive one-partition-per-bin grouping.
        let u = vec![
            Unit { unit_id: "a".into(), size: 10, metadata: 0u32.to_le_bytes().to_vec() },
            Unit { unit_id: "b".into(), size: 4, metadata: 0u32.to_le_bytes().to_vec() },
            Unit { unit_id: "c".into(), size: 3, metadata: 0u32.to_le_bytes().to_vec() },
            Unit { unit_id: "d".into(), size: 2, metadata: 0u32.to_le_bytes().to_vec() },
        ];
        let stripes = IvfCentroidPacker.pack(3, &u);
        assert_eq!(stripes.len(), 1, "4 small same-cluster units under k=3 should fit in one stripe, got {}", stripes.len());
        let total_units: usize = stripes[0].bins.iter().map(|b| b.len()).sum();
        assert_eq!(total_units, 4);
    }
}
