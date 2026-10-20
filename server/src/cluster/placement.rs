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

    /// Phase 5 regression gate for the extensibility claim this module's
    /// own doc makes ("a second implementation... can be dropped in
    /// later without changing coordinator.rs"). A second, deliberately
    /// non-realistic policy (fixed reverse-order, not content-aware or
    /// churn-minimizing like `ComputedPlacement`) is implemented here,
    /// with zero changes to `coordinator.rs` -- it only ever calls
    /// `state.placement.place(...)` through the `Arc<dyn PlacementPolicy>`
    /// trait object on `ClusterState`, never `ComputedPlacement` by name,
    /// so any implementation of this trait is interchangeable there by
    /// construction. This proves the policy itself is genuinely
    /// pluggable and produces correct (just different) output, not that
    /// `coordinator.rs` was touched to accommodate it.
    ///
    /// Not covered here: a full round trip through `put_object_plain`
    /// itself needs real peer HTTP servers (location-pin replication
    /// makes real network calls to every peer and requires all of them
    /// to ack) -- out of reach for an in-process unit test without new
    /// mock-server infrastructure. That's a real gap in test depth, not
    /// papered over: this test instead proves extensibility at the
    /// `PlacementPolicy`/`ClusterState` boundary, which is the actual
    /// mechanism the claim rests on.
    struct ReversePlacement;
    impl PlacementPolicy for ReversePlacement {
        fn place(&self, _bucket: &str, _key: &str, peers: &[String], k: usize, m: usize) -> Vec<String> {
            let mut reversed: Vec<String> = peers.iter().rev().cloned().collect();
            reversed.truncate(k + m);
            reversed
        }
    }

    #[test]
    fn a_second_placement_policy_drops_in_with_zero_coordinator_changes() {
        let p = peers(6);

        let computed = ComputedPlacement::new().place("bucket1", "key1", &p, 3, 2);
        let reversed = ReversePlacement.place("bucket1", "key1", &p, 3, 2);

        // Correct per its own (trivial) algorithm...
        assert_eq!(reversed, vec![p[5].clone(), p[4].clone(), p[3].clone(), p[2].clone(), p[1].clone()]);
        assert_eq!(reversed.len(), 5);
        // ...and genuinely a different policy, not accidentally
        // reimplementing ComputedPlacement's own HRW scoring.
        assert_ne!(computed, reversed);

        // The actual extensibility proof: both are usable, interchangeably,
        // anywhere `coordinator.rs` holds an `Arc<dyn PlacementPolicy>`
        // (e.g. `ClusterState::placement`) -- neither this cast nor
        // `coordinator.rs` needed to change for the second policy to
        // compile and run here.
        let policies: Vec<std::sync::Arc<dyn PlacementPolicy>> =
            vec![std::sync::Arc::new(ComputedPlacement::new()), std::sync::Arc::new(ReversePlacement)];
        for policy in &policies {
            assert_eq!(policy.place("bucket1", "key1", &p, 3, 2).len(), 5);
        }
    }
}
