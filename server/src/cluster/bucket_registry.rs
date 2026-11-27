//! Who owns a bucket, cluster-wide, independent of whoever's asking —
//! the prerequisite RFC2.13 (S3 bucket policy) needs and the cluster
//! doesn't have yet. Every other cluster store (`location_store.rs`,
//! `content_location_store.rs`) is keyed by `(user_id, bucket, key)`: the
//! *caller's own* identity is baked into the key, so there is no way for
//! one user's request to even look up a bucket owned by someone else —
//! not a permission problem, an addressability one. Bucket policy's whole
//! point is granting access to a principal who *isn't* the owner, so it
//! needs a bucket-keyed (not user_id-keyed) answer to "who actually owns
//! this." This store is that answer, plus a slot to hold the (for now,
//! unevaluated) policy JSON once RFC2.13's evaluator exists.
//!
//! Not purely hypothetical: two different OIDC identities' `buckets`
//! claims (see `s3/sts.rs`) are set independently by whoever configures
//! each IdP user, with nothing in WarpDrive coordinating them — two
//! different users' sessions can genuinely be scoped to a bucket *name*
//! that collides. `S3AuthResult.allowed_buckets` (is this session allowed
//! to attempt this bucket name at all, per its own claims) and this
//! registry (does this user_id actually own that bucket in WarpDrive's
//! data model) are deliberately two separate, orthogonal checks.
//!
//! Same Bitcask append-only-log pattern as `bucket_config.rs`, which this
//! module mirrors closely: bucket-name-only key (no `user_id` dimension —
//! that's the point), `DurableAppendLog` + in-memory `HashMap` index. Same
//! consistency bar too: a bucket's owner is at least as consistency-
//! critical as its placement config, so claiming one is synchronously
//! replicated to *every* peer, all of which must agree (`coordinator.rs`'s
//! `cluster_claim_bucket`) — not just quorum, for the same reason
//! `cluster_put_bucket_config` isn't: an operator-visible split ("which
//! user_id does peer A think owns this bucket, vs peer B") is a
//! consistency bug, not a tolerable availability tradeoff.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};
use parking_lot::RwLock;
use crate::storage::durable_log::DurableAppendLog;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BucketOwnership {
    pub bucket: String,
    pub owner_user_id: String,
    /// Raw policy JSON, stored but not yet evaluated anywhere — RFC2.13's
    /// Effect/Principal/Action/Resource/Condition engine is a separate,
    /// later phase. This field exists so that phase has somewhere to read
    /// from and write to on day one, instead of needing its own schema
    /// migration.
    #[serde(default)]
    pub policy_json: Option<String>,
}

/// Result of attempting to claim an unclaimed bucket name for `user_id`.
#[derive(Debug, Clone, PartialEq)]
pub enum ClaimResult {
    /// No prior owner — `user_id` is now it.
    Claimed,
    /// Someone already owns this bucket — might be `user_id` themselves
    /// (an idempotent re-claim, e.g. a second PUT to a bucket they already
    /// created) or a genuinely different owner (the cross-principal case).
    AlreadyOwnedBy(String),
}

pub trait BucketRegistry: Send + Sync {
    /// Atomic, local-node-only insert-if-absent: if `bucket` has no entry
    /// yet, creates one owned by `user_id` and returns `Claimed`.
    /// Otherwise returns the existing owner unchanged (`AlreadyOwnedBy`),
    /// idempotently — this call never overwrites an existing owner.
    /// Cluster-wide correctness (every peer agreeing on the same owner)
    /// is `cluster_claim_bucket`'s job (`coordinator.rs`), not this trait's
    /// — this method is deliberately single-node-local and cheap.
    fn claim_if_absent(&self, bucket: &str, user_id: &str) -> ClaimResult;
    fn get(&self, bucket: &str) -> Option<BucketOwnership>;
    /// Owner-only to call (enforced by the caller, same as every other
    /// bucket-level setting in this project) — just a raw string
    /// round-trip, no parsing. `None` clears any existing policy.
    fn put_policy(&self, bucket: &str, policy_json: Option<String>) -> io::Result<()>;
}

pub struct BitcaskBucketRegistry {
    log: DurableAppendLog,
    index: RwLock<HashMap<String, BucketOwnership>>,
    /// Same #155 reasoning as `bucket_config.rs`'s matching field: one lock
    /// held across both the log append and the index update, so two
    /// concurrent local calls (a claim racing a policy write, or two
    /// claims racing each other) can never apply to the log and the index
    /// in different orders.
    write_lock: parking_lot::Mutex<()>,
}

impl BitcaskBucketRegistry {
    pub fn open(log_path: impl AsRef<Path>) -> io::Result<Self> {
        let log_path: PathBuf = log_path.as_ref().to_path_buf();
        if let Some(parent) = log_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut index = HashMap::new();
        if log_path.exists() {
            let f = File::open(&log_path)?;
            for line in BufReader::new(f).lines() {
                let line = line?;
                if line.is_empty() {
                    continue;
                }
                let entry: BucketOwnership =
                    serde_json::from_str(&line).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                index.insert(entry.bucket.clone(), entry);
            }
        }

        let log = DurableAppendLog::open(&log_path)?;
        Ok(Self {
            log,
            index: RwLock::new(index),
            write_lock: parking_lot::Mutex::new(()),
        })
    }

    fn append_and_index(&self, entry: &BucketOwnership) -> io::Result<()> {
        let line = serde_json::to_string(entry).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let ticket = {
            let _write_guard = self.write_lock.lock();
            let ticket = self.log.append_line(&line)?;
            self.index.write().insert(entry.bucket.clone(), entry.clone());
            ticket
        };
        self.log.sync_ticket(ticket)
    }
}

impl BucketRegistry for BitcaskBucketRegistry {
    fn claim_if_absent(&self, bucket: &str, user_id: &str) -> ClaimResult {
        // Held across the existence check and the (potential) insert, same
        // as `put`'s lock scope elsewhere in this project — otherwise two
        // threads on this one node could both observe "absent" and both
        // append a claim, which the index would then resolve arbitrarily
        // (last-insert-wins) while the log replay on restart could resolve
        // it differently, diverging live state from a fresh replay.
        let _write_guard = self.write_lock.lock();
        if let Some(existing) = self.index.read().get(bucket) {
            return ClaimResult::AlreadyOwnedBy(existing.owner_user_id.clone());
        }
        let entry = BucketOwnership { bucket: bucket.to_string(), owner_user_id: user_id.to_string(), policy_json: None };
        let line = match serde_json::to_string(&entry) {
            Ok(l) => l,
            Err(_) => return ClaimResult::AlreadyOwnedBy(user_id.to_string()), // unreachable in practice; never silently "unclaim"
        };
        let ticket = match self.log.append_line(&line) {
            Ok(t) => t,
            Err(_) => return ClaimResult::AlreadyOwnedBy(user_id.to_string()),
        };
        self.index.write().insert(bucket.to_string(), entry);
        let _ = self.log.sync_ticket(ticket);
        ClaimResult::Claimed
    }

    fn get(&self, bucket: &str) -> Option<BucketOwnership> {
        self.index.read().get(bucket).cloned()
    }

    fn put_policy(&self, bucket: &str, policy_json: Option<String>) -> io::Result<()> {
        let mut entry = self
            .index
            .read()
            .get(bucket)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("bucket {bucket} has no owner yet")))?;
        entry.policy_json = policy_json;
        self.append_and_index(&entry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claim_if_absent_succeeds_once() {
        let dir = tempfile::tempdir().unwrap();
        let store = BitcaskBucketRegistry::open(dir.path().join("br.log")).unwrap();
        assert_eq!(store.claim_if_absent("b1", "alice"), ClaimResult::Claimed);
        assert_eq!(store.get("b1").unwrap().owner_user_id, "alice");
    }

    #[test]
    fn claim_if_absent_is_idempotent_for_the_real_owner() {
        let dir = tempfile::tempdir().unwrap();
        let store = BitcaskBucketRegistry::open(dir.path().join("br.log")).unwrap();
        assert_eq!(store.claim_if_absent("b1", "alice"), ClaimResult::Claimed);
        assert_eq!(store.claim_if_absent("b1", "alice"), ClaimResult::AlreadyOwnedBy("alice".to_string()));
    }

    #[test]
    fn claim_if_absent_rejects_a_different_user() {
        let dir = tempfile::tempdir().unwrap();
        let store = BitcaskBucketRegistry::open(dir.path().join("br.log")).unwrap();
        assert_eq!(store.claim_if_absent("b1", "alice"), ClaimResult::Claimed);
        assert_eq!(store.claim_if_absent("b1", "bob"), ClaimResult::AlreadyOwnedBy("alice".to_string()));
        // Rejection must not have mutated the stored owner.
        assert_eq!(store.get("b1").unwrap().owner_user_id, "alice");
    }

    #[test]
    fn missing_bucket_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = BitcaskBucketRegistry::open(dir.path().join("br.log")).unwrap();
        assert!(store.get("never-claimed").is_none());
    }

    #[test]
    fn restart_rebuilds_index_from_log_replay() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("br.log");
        {
            let store = BitcaskBucketRegistry::open(&log_path).unwrap();
            assert_eq!(store.claim_if_absent("b1", "alice"), ClaimResult::Claimed);
        }
        let reopened = BitcaskBucketRegistry::open(&log_path).unwrap();
        assert_eq!(reopened.get("b1").unwrap().owner_user_id, "alice");
    }

    #[test]
    fn put_policy_round_trips_and_requires_an_existing_owner() {
        let dir = tempfile::tempdir().unwrap();
        let store = BitcaskBucketRegistry::open(dir.path().join("br.log")).unwrap();

        // No owner yet -> error, not a silent no-op.
        assert!(store.put_policy("b1", Some("{}".to_string())).is_err());

        store.claim_if_absent("b1", "alice");
        store.put_policy("b1", Some(r#"{"Version":"2012-10-17"}"#.to_string())).unwrap();
        assert_eq!(store.get("b1").unwrap().policy_json, Some(r#"{"Version":"2012-10-17"}"#.to_string()));
        // Ownership must survive a policy update.
        assert_eq!(store.get("b1").unwrap().owner_user_id, "alice");

        store.put_policy("b1", None).unwrap();
        assert_eq!(store.get("b1").unwrap().policy_json, None);
    }

    #[test]
    fn restart_rebuilds_policy_too() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("br.log");
        {
            let store = BitcaskBucketRegistry::open(&log_path).unwrap();
            store.claim_if_absent("b1", "alice");
            store.put_policy("b1", Some("{\"p\":1}".to_string())).unwrap();
        }
        let reopened = BitcaskBucketRegistry::open(&log_path).unwrap();
        let entry = reopened.get("b1").unwrap();
        assert_eq!(entry.owner_user_id, "alice");
        assert_eq!(entry.policy_json, Some("{\"p\":1}".to_string()));
    }

    /// The core algorithmic property, exercised directly on one node (the
    /// cluster-wide version of this is `cluster_claim_bucket`'s all-peer-
    /// agreement requirement in coordinator.rs, which this is the local
    /// primitive for): many threads racing to claim the same brand-new
    /// bucket name for *different* users must resolve to exactly one
    /// winner, and every loser must see that same winner, not a mix.
    #[test]
    fn concurrent_claims_for_a_new_bucket_resolve_to_exactly_one_winner() {
        use std::thread;

        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(BitcaskBucketRegistry::open(dir.path().join("br.log")).unwrap());

        let handles: Vec<_> = (0..16)
            .map(|t| {
                let store = std::sync::Arc::clone(&store);
                thread::spawn(move || store.claim_if_absent("racybucket", &format!("user{t}")))
            })
            .collect();

        let results: Vec<ClaimResult> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let claimed_count = results.iter().filter(|r| **r == ClaimResult::Claimed).count();
        assert_eq!(claimed_count, 1, "exactly one thread's claim_if_absent call must report Claimed");

        let winner = store.get("racybucket").unwrap().owner_user_id;
        for r in &results {
            if let ClaimResult::AlreadyOwnedBy(owner) = r {
                assert_eq!(owner, &winner, "every loser must see the same winner as the stored owner");
            }
        }

        let replayed = BitcaskBucketRegistry::open(dir.path().join("br.log")).unwrap().get("racybucket");
        assert_eq!(replayed.unwrap().owner_user_id, winner, "log replay must agree with the live index");
    }
}
