//! Per-node shard-byte metadata (shard_key -> (offset, size)), Bitcask-style
//! — the same append-only-log-plus-in-memory-index pattern as
//! `location_store.rs`, applied one layer deeper.
//!
//! **Why this exists.** `location_store.rs` replaced SQLite for the
//! cluster's placement pin. The shard *bytes* themselves still went
//! through the existing single-node `MetadataService`, which serializes
//! every call through one process-wide `lazy_static Arc<Mutex<Connection>>`
//! (`metadata/sqlite_store.rs`) — exactly the single-writer bottleneck the
//! user originally flagged, just one layer deeper than the layer that got
//! fixed first. A GCP load test confirmed it: throughput stayed flat
//! (~980 req/s) going from 64 to 256 concurrent clients on a 24-core VM,
//! evidence of a serialization point, not a CPU limit. This module removes
//! that mutex from the cluster path entirely. WarpDrive's existing
//! single-node SQLite metadata layer is untouched and keeps serving
//! single-node deployments exactly as before — this is a separate,
//! cluster-only store, same as `location_store.rs`.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

pub trait ShardMetaStore: Send + Sync {
    fn put(&self, shard_key: &str, offset: u64, size: u64) -> io::Result<()>;
    fn get(&self, shard_key: &str) -> Option<(u64, u64)>;
}

/// Append-only log of `shard_key,offset,size` lines; in-memory index
/// rebuilt by replaying the log on startup. No tombstones — shard keys are
/// never deleted independently of the whole object (see the non-goal on
/// decode-from-failure / steady-state GC in the architecture doc), so a
/// later `put` for the same key (an overwrite) is the only thing that can
/// change what a key maps to, same as `location_store.rs`'s pin.
pub struct BitcaskShardMetaStore {
    log_path: PathBuf,
    log_file: RwLock<File>,
    index: RwLock<HashMap<String, (u64, u64)>>,
}

impl BitcaskShardMetaStore {
    pub fn open(log_path: impl AsRef<Path>) -> io::Result<Self> {
        let log_path = log_path.as_ref().to_path_buf();
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
                let mut parts = line.splitn(3, ',');
                let key = parts.next().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing key"))?;
                let offset: u64 = parts
                    .next()
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad offset"))?;
                let size: u64 = parts
                    .next()
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad size"))?;
                index.insert(key.to_string(), (offset, size));
            }
        }

        let log_file = OpenOptions::new().create(true).append(true).open(&log_path)?;
        Ok(Self {
            log_path,
            log_file: RwLock::new(log_file),
            index: RwLock::new(index),
        })
    }

    pub fn path(&self) -> &Path {
        &self.log_path
    }
}

impl ShardMetaStore for BitcaskShardMetaStore {
    fn put(&self, shard_key: &str, offset: u64, size: u64) -> io::Result<()> {
        {
            let mut f = self.log_file.write().unwrap();
            writeln!(f, "{shard_key},{offset},{size}")?;
            f.flush()?;
        }
        self.index.write().unwrap().insert(shard_key.to_string(), (offset, size));
        Ok(())
    }

    fn get(&self, shard_key: &str) -> Option<(u64, u64)> {
        self.index.read().unwrap().get(shard_key).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_then_get_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let store = BitcaskShardMetaStore::open(dir.path().join("shardmeta.log")).unwrap();
        store.put("bucket__key__shard0", 128, 4096).unwrap();
        assert_eq!(store.get("bucket__key__shard0"), Some((128, 4096)));
    }

    #[test]
    fn restart_rebuilds_index_from_log_replay() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("shardmeta.log");
        {
            let store = BitcaskShardMetaStore::open(&log_path).unwrap();
            store.put("k1", 0, 100).unwrap();
            store.put("k2", 100, 200).unwrap();
        }
        let reopened = BitcaskShardMetaStore::open(&log_path).unwrap();
        assert_eq!(reopened.get("k1"), Some((0, 100)));
        assert_eq!(reopened.get("k2"), Some((100, 200)));
    }

    #[test]
    fn overwrite_updates_the_mapping() {
        let dir = tempfile::tempdir().unwrap();
        let store = BitcaskShardMetaStore::open(dir.path().join("shardmeta.log")).unwrap();
        store.put("k1", 0, 100).unwrap();
        store.put("k1", 500, 300).unwrap();
        assert_eq!(store.get("k1"), Some((500, 300)));
    }
}
