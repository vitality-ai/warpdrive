//! Placement policy behind a trait, so a second implementation
//! (content-dependent, Fusion-style bin-packing) can be dropped in later
//! without changing coordinator.rs or the wire format. Only
//! `ComputedPlacement` is implemented now.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

pub trait PlacementPolicy: Send + Sync {
    /// Choose `k+m` peers for `(bucket, key)` out of the given peer set.
    /// Deterministic: same inputs always produce the same output.
    fn place(&self, bucket: &str, key: &str, peers: &[String], k: usize, m: usize) -> Vec<String>;
}

/// CRUSH-style deterministic placement using rendezvous (highest-random-
/// weight) hashing: each peer gets a score for (bucket, key), and the top
/// k+m peers by score are chosen. Deliberately not a modulo hash — modulo
/// reshuffles almost every key when the peer count changes; HRW only
/// reassigns the keys that would have gone to the peer that joined or left.
pub struct ComputedPlacement;

impl ComputedPlacement {
    pub fn new() -> Self {
        Self
    }

    fn score(bucket: &str, key: &str, peer: &str) -> u64 {
        let mut hasher = DefaultHasher::new();
        bucket.hash(&mut hasher);
        key.hash(&mut hasher);
        peer.hash(&mut hasher);
        hasher.finish()
    }
}

impl Default for ComputedPlacement {
    fn default() -> Self {
        Self::new()
    }
}

impl PlacementPolicy for ComputedPlacement {
    fn place(&self, bucket: &str, key: &str, peers: &[String], k: usize, m: usize) -> Vec<String> {
        let want = k + m;
        let mut scored: Vec<(u64, &String)> = peers
            .iter()
            .map(|p| (Self::score(bucket, key, p), p))
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
        scored.into_iter().take(want).map(|(_, p)| p.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peers(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("http://node{i}:9710")).collect()
    }

    #[test]
    fn deterministic_same_inputs_same_output() {
        let policy = ComputedPlacement::new();
        let p = peers(6);
        let a = policy.place("bucket1", "key1", &p, 3, 2);
        let b = policy.place("bucket1", "key1", &p, 3, 2);
        assert_eq!(a, b);
        assert_eq!(a.len(), 5);
    }

    #[test]
    fn hrw_minimizes_churn_on_peer_join() {
        let policy = ComputedPlacement::new();
        let before = peers(6);
        let before_placement = policy.place("bucket1", "key1", &before, 3, 2);

        let mut after = before.clone();
        after.push("http://node6:9710".to_string());
        let after_placement = policy.place("bucket1", "key1", &after, 3, 2);

        // Adding one node can displace at most the single lowest-ranked
        // member of the previous set, never a wholesale reshuffle.
        let still_present = before_placement
            .iter()
            .filter(|p| after_placement.contains(p))
            .count();
        assert!(still_present >= before_placement.len() - 1);
    }

    #[test]
    fn different_keys_can_map_to_different_peer_sets() {
        let policy = ComputedPlacement::new();
        let p = peers(10);
        let a = policy.place("bucket1", "key1", &p, 3, 2);
        let b = policy.place("bucket1", "key2", &p, 3, 2);
        assert_ne!(a, b);
    }
}
