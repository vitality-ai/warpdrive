//! The contract content-dependent placement needs from a bin-packing
//! strategy — not a specific algorithm. `FacPacker` (below) implements it
//! using Fusion's Algorithm 1, generalized (Lu, Raina, Cidon, Freedman,
//! ASPLOS'25), ported faithfully from this project's own `fac_core.py`.
//! Anyone could implement `StripePacker` with a different strategy without
//! touching `ContentDependentPlacement`, the EC layer, or `coordinator.rs`
//! at all — the same reason every other component here (`Storage`,
//! `PlacementPolicy`, `LocationStore`, `ErasureCoder`, `PeerClient`) is a
//! trait with one concrete implementation, not a concrete type.

#[derive(Debug, Clone)]
pub struct Unit {
    pub unit_id: String,
    pub size: usize,
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

/// FAC: Fusion's Algorithm 1, generalized — the one implementation shipped
/// here. Greedy: seed each stripe with the largest remaining unit, pack
/// the rest into whichever other bin is least full and still has room
/// under the seed's size (the stripe's `capacity`).
pub struct FacPacker;

impl StripePacker for FacPacker {
    fn pack(&self, k: usize, units: &[Unit]) -> Vec<Stripe> {
        let mut remaining: Vec<&Unit> = units.iter().collect();
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn units(sizes: &[usize]) -> Vec<Unit> {
        sizes
            .iter()
            .enumerate()
            .map(|(i, &size)| Unit { unit_id: format!("u{i}"), size })
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
}
