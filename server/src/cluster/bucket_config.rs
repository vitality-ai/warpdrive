//! Per-bucket placement configuration: which `StripePacker` a bucket uses
//! (a registry name, e.g. "fac" — keeps the *choice* of algorithm a config
//! value, not a hardcoded dispatch, so a third, user-defined packer only
//! needs to implement `StripePacker` and be registered in
//! `ClusterState::packers`, same as `FacPacker`), and the maximum
//! additional storage overhead (vs. optimal) the bucket will tolerate
//! before a PUT falls back to plain, non-content-dependent erasure coding.
//!
//! The overhead threshold is Fusion's own mechanism (ASPLOS'25 §4.2/§6):
//! "a system-level hyperparameter... allowing users to specify the maximum
//! additional storage overhead they can tolerate... If the algorithm
//! cannot construct stripes within the specified storage budget, it
//! defaults to erasure coding the object into fixed-sized blocks." Their
//! evaluation sets it to 2% globally. We make it per-bucket instead of
//! global, matching how every other bucket-level setting in this project
//! works (versioning, ACL, object-lock retention) — a safety/explainability
//! knob the operator sets once per bucket, not a single cluster-wide
//! constant.
//!
//! Same Bitcask append-only-log pattern as `location_store.rs` and
//! `content_location_store.rs`. Writes here are rare (an operator setting
//! a bucket's policy) and must be visible to whichever node ends up
//! coordinating a PUT, so `cluster_put_bucket_config` (coordinator.rs)
//! replicates synchronously to *every* known peer and requires all of them
//! to ack — unlike shard/location writes, which only need quorum, this is
//! small, infrequent config data where correctness (every node agreeing on
//! the policy) matters more than availability during a partition.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};
use parking_lot::RwLock;
use crate::storage::durable_log::DurableAppendLog;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BucketPlacementConfig {
    pub bucket: String,
    /// Registry key into `ClusterState::packers`. An unknown name is not a
    /// hard error at PUT time — it degrades to the plain EC path with a
    /// logged warning, since a bad config value should never be the reason
    /// a write fails (see `coordinator.rs`'s dispatch).
    pub packer_name: String,
    /// Max tolerated storage overhead (%) vs. optimal MDS size. Fusion's
    /// own evaluation default is 2.0.
    pub overhead_threshold_pct: f64,
}

/// The bucket has no explicit config — matches Fusion's own evaluation
/// default (`packer_name: "fac"`, `overhead_threshold_pct: 2.0`) so
/// existing behavior (content-dependent placement whenever the header is
/// present) is unchanged until an operator opts into something else.
pub fn default_bucket_config(bucket: &str) -> BucketPlacementConfig {
    BucketPlacementConfig {
        bucket: bucket.to_string(),
        packer_name: "fac".to_string(),
        overhead_threshold_pct: 2.0,
    }
}

pub trait BucketConfigStore: Send + Sync {
    fn put(&self, config: BucketPlacementConfig) -> io::Result<()>;
    fn get(&self, bucket: &str) -> Option<BucketPlacementConfig>;
}

pub struct BitcaskBucketConfigStore {
    /// Owns the file, the dup'd sync-only handle, and the group-commit
    /// batching needed to make an append durable without paying one
    /// fsync per write -- see `storage::durable_log`'s module doc. This
    /// store's own doc above explains why durability matters here more
    /// than for shard/location writes: a bucket-config write is
    /// synchronously replicated to every peer and all of them must ack,
    /// so a crash that silently loses it on one node after a full-quorum
    /// ack is exactly the gap group-commit exists to close.
    log: DurableAppendLog,
    index: RwLock<HashMap<String, BucketPlacementConfig>>,
    /// #155 — see `location_store.rs`'s matching field doc: without one
    /// lock held across both the log append and the index update, two
    /// concurrent `put`s for the same bucket (e.g. two operators changing
    /// its config at once) could apply to the log and the index in
    /// different orders, so a restart (log replay only) can disagree
    /// with what the live index had.
    write_lock: parking_lot::Mutex<()>,
}

impl BitcaskBucketConfigStore {
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
                let config: BucketPlacementConfig =
                    serde_json::from_str(&line).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                index.insert(config.bucket.clone(), config);
            }
        }

        let log = DurableAppendLog::open(&log_path)?;
        Ok(Self {
            log,
            index: RwLock::new(index),
            write_lock: parking_lot::Mutex::new(()),
        })
    }
}

impl BucketConfigStore for BitcaskBucketConfigStore {
    fn put(&self, config: BucketPlacementConfig) -> io::Result<()> {
        let line = serde_json::to_string(&config).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        // #155: held across both the append and the index update, so two
        // concurrent puts for the same bucket can never apply to the log
        // and the index in different orders. Released before the
        // durability wait below, same reasoning as location_store.rs's
        // `put`.
        let ticket = {
            let _write_guard = self.write_lock.lock();
            let ticket = self.log.append_line(&line)?;
            self.index.write().insert(config.bucket.clone(), config);
            ticket
        };
        self.log.sync_ticket(ticket)
    }

    fn get(&self, bucket: &str) -> Option<BucketPlacementConfig> {
        self.index.read().get(bucket).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_then_get_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let store = BitcaskBucketConfigStore::open(dir.path().join("bc.log")).unwrap();
        let cfg = BucketPlacementConfig {
            bucket: "b1".into(),
            packer_name: "fac".into(),
            overhead_threshold_pct: 5.0,
        };
        store.put(cfg.clone()).unwrap();
        assert_eq!(store.get("b1").unwrap(), cfg);
    }

    #[test]
    fn missing_bucket_returns_none_so_caller_can_apply_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let store = BitcaskBucketConfigStore::open(dir.path().join("bc.log")).unwrap();
        assert!(store.get("never-configured").is_none());
    }

    #[test]
    fn restart_rebuilds_index_from_log_replay() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("bc.log");
        {
            let store = BitcaskBucketConfigStore::open(&log_path).unwrap();
            store
                .put(BucketPlacementConfig {
                    bucket: "b1".into(),
                    packer_name: "fac".into(),
                    overhead_threshold_pct: 2.0,
                })
                .unwrap();
        }
        let reopened = BitcaskBucketConfigStore::open(&log_path).unwrap();
        assert_eq!(reopened.get("b1").unwrap().overhead_threshold_pct, 2.0);
    }

    #[test]
    fn later_put_overwrites_earlier_config_for_the_same_bucket() {
        let dir = tempfile::tempdir().unwrap();
        let store = BitcaskBucketConfigStore::open(dir.path().join("bc.log")).unwrap();
        store
            .put(BucketPlacementConfig { bucket: "b1".into(), packer_name: "fac".into(), overhead_threshold_pct: 2.0 })
            .unwrap();
        store
            .put(BucketPlacementConfig { bucket: "b1".into(), packer_name: "fac".into(), overhead_threshold_pct: 10.0 })
            .unwrap();
        assert_eq!(store.get("b1").unwrap().overhead_threshold_pct, 10.0);
    }

    /// Same invariant and same reasoning as `location_store.rs`'s matching
    /// test (#155): the live index and a fresh replay of the log must
    /// always agree about which concurrent put for the same bucket won,
    /// however the race actually resolved.
    #[test]
    fn concurrent_puts_to_the_same_bucket_never_diverge_log_from_index() {
        use std::thread;

        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("bc.log");
        let store = std::sync::Arc::new(BitcaskBucketConfigStore::open(&log_path).unwrap());

        let handles: Vec<_> = (0..16)
            .map(|t| {
                let store = std::sync::Arc::clone(&store);
                thread::spawn(move || {
                    for i in 0..50 {
                        store
                            .put(BucketPlacementConfig {
                                bucket: "racybucket".into(),
                                packer_name: "fac".into(),
                                overhead_threshold_pct: (t * 1000 + i) as f64,
                            })
                            .unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let live = store.get("racybucket");
        let replayed = BitcaskBucketConfigStore::open(&log_path).unwrap().get("racybucket");
        assert_eq!(live, replayed, "live index and a fresh log replay must agree");
    }
}
