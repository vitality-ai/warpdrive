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
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

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
    log_file: RwLock<File>,
    index: RwLock<HashMap<String, BucketPlacementConfig>>,
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

        let log_file = OpenOptions::new().create(true).append(true).open(&log_path)?;
        Ok(Self {
            log_file: RwLock::new(log_file),
            index: RwLock::new(index),
        })
    }
}

impl BucketConfigStore for BitcaskBucketConfigStore {
    fn put(&self, config: BucketPlacementConfig) -> io::Result<()> {
        let line = serde_json::to_string(&config).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        {
            let mut f = self.log_file.write().unwrap();
            writeln!(f, "{line}")?;
            f.flush()?;
        }
        self.index.write().unwrap().insert(config.bucket.clone(), config);
        Ok(())
    }

    fn get(&self, bucket: &str) -> Option<BucketPlacementConfig> {
        self.index.read().unwrap().get(bucket).cloned()
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
}
