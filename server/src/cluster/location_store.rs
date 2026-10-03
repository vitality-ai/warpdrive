//! Not SQLite. Append-only log (Bitcask-style) + in-memory index, so a
//! cluster node doesn't inherit a single-writer SQLite bottleneck for its
//! per-object shard-placement map. WarpDrive's existing single-node SQLite
//! metadata layer (`metadata/sqlite_store.rs`) is untouched and keeps
//! serving single-node deployments exactly as today.
//!
//! This is also where object lock (WORM retention) lives for the cluster
//! path: `retention_mode`/`retain_until`/`legal_hold` sit on the *same*
//! per-key record as the shard-placement pin, so there is exactly one
//! lookup for "where are this object's shards" and "is this object
//! currently locked" — not two systems that can drift out of sync. See
//! docs/Distributed-Engine-Plan.md.
//!
//! PUT resolves placement fresh and writes the pin here. GET and DELETE
//! read the pin instead of recomputing `ComputedPlacement` — this is what
//! makes "existing objects are never moved when the peer list changes"
//! actually true, not just asserted.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LocationRecord {
    pub bucket: String,
    pub key: String,
    /// Index i = the peer holding shard i (data shards first, then parity).
    pub shard_peers: Vec<String>,
    pub k: usize,
    pub m: usize,
    pub original_len: usize,
    pub retention_mode: Option<String>, // "GOVERNANCE" | "COMPLIANCE"
    pub retain_until: Option<String>,   // RFC3339
    pub legal_hold: bool,
}

pub trait LocationStore: Send + Sync {
    fn put(&self, record: LocationRecord) -> io::Result<()>;
    fn get(&self, bucket: &str, key: &str) -> Option<LocationRecord>;
    fn delete(&self, bucket: &str, key: &str) -> io::Result<()>;
    /// Keys (with their byte size) in `bucket` whose key starts with
    /// `prefix` — the minimal S3 ListObjectsV2 surface needs this (object
    /// stores speaking the real S3 protocol, like Lance's client, list a
    /// dataset's files; the raw `/cluster/{bucket}/{key}` API never needed
    /// this since callers always know their own key).
    fn list(&self, bucket: &str, prefix: &str) -> Vec<(String, u64)>;
    /// Rewrites the log to hold only the current live entries (one line
    /// per in-memory index entry, no tombstones -- there's nothing left
    /// for them to cancel out once the dead entries they were cancelling
    /// are gone too), then atomically replaces the old log file. Bounds
    /// both on-disk size and the next restart's replay time to the number
    /// of *live* objects, not the total number of historical writes/
    /// deletes/overwrites ever made (#163: an append-only log with no
    /// compaction grows without bound under real churn).
    fn compact(&self) -> io::Result<()>;
}

fn record_key(bucket: &str, key: &str) -> String {
    format!("{bucket}/{key}")
}

/// Append-only log of JSON-lines `(record_key, Option<LocationRecord>)`
/// pairs; `None` is a tombstone (delete). In-memory index is rebuilt by
/// replaying the whole log on startup. `BTreeMap`, not `HashMap` (#163):
/// `record_key` is `"{bucket}/{key}"`, so lexicographic order groups every
/// bucket's keys contiguously, which is what makes `list`'s prefix lookup
/// a bounded range scan instead of a full-table scan.
pub struct BitcaskLocationStore {
    log_path: PathBuf,
    log_file: RwLock<File>,
    index: RwLock<BTreeMap<String, LocationRecord>>,
}

impl BitcaskLocationStore {
    pub fn open(log_path: impl AsRef<Path>) -> io::Result<Self> {
        let log_path = log_path.as_ref().to_path_buf();
        if let Some(parent) = log_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut index = BTreeMap::new();
        if log_path.exists() {
            let f = File::open(&log_path)?;
            for line in BufReader::new(f).lines() {
                let line = line?;
                if line.is_empty() {
                    continue;
                }
                let (rk, record): (String, Option<LocationRecord>) = serde_json::from_str(&line)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                match record {
                    Some(r) => {
                        index.insert(rk, r);
                    }
                    None => {
                        index.remove(&rk);
                    }
                }
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

    fn append_line(&self, line: &str) -> io::Result<()> {
        let mut f = self.log_file.write().unwrap();
        writeln!(f, "{line}")?;
        f.flush()
    }
}

impl LocationStore for BitcaskLocationStore {
    fn put(&self, record: LocationRecord) -> io::Result<()> {
        let rk = record_key(&record.bucket, &record.key);
        let line = serde_json::to_string(&(rk.clone(), Some(record.clone())))
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        self.append_line(&line)?;
        self.index.write().unwrap().insert(rk, record);
        Ok(())
    }

    fn get(&self, bucket: &str, key: &str) -> Option<LocationRecord> {
        self.index.read().unwrap().get(&record_key(bucket, key)).cloned()
    }

    fn delete(&self, bucket: &str, key: &str) -> io::Result<()> {
        let rk = record_key(bucket, key);
        let line = serde_json::to_string(&(rk.clone(), Option::<LocationRecord>::None))
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        self.append_line(&line)?;
        self.index.write().unwrap().remove(&rk);
        Ok(())
    }

    fn list(&self, bucket: &str, prefix: &str) -> Vec<(String, u64)> {
        let bucket_prefix = format!("{bucket}/");
        let full_prefix = format!("{bucket_prefix}{prefix}");
        // A BTreeMap range starting *at* the prefix (an O(log n) binary
        // search, not a linear scan from the beginning) then taken only
        // while keys still match it: visits exactly the matching keys, in
        // order, and stops as soon as the first non-matching one is seen.
        // O(log n + matches), not O(every key in every bucket) like the
        // old full-iterate-and-filter did (#163).
        self.index
            .read()
            .unwrap()
            .range(full_prefix.clone()..)
            .take_while(|(k, _)| k.starts_with(&full_prefix))
            .map(|(k, v)| (k[bucket_prefix.len()..].to_string(), v.original_len as u64))
            .collect()
    }

    fn compact(&self) -> io::Result<()> {
        // Hold both locks for the duration: a concurrent put/delete must
        // see either the pre- or post-compaction state, never write to a
        // log file mid-rewrite out from under it.
        let index = self.index.read().unwrap();
        let mut log_file = self.log_file.write().unwrap();

        let tmp_path = self.log_path.with_extension("compact.tmp");
        {
            let mut tmp = File::create(&tmp_path)?;
            for (rk, record) in index.iter() {
                let line = serde_json::to_string(&(rk.clone(), Some(record.clone())))
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                writeln!(tmp, "{line}")?;
            }
            tmp.flush()?;
        }
        // Atomic on the same filesystem (same directory as log_path, not
        // /tmp): a crash here leaves either the old log or the new one
        // fully intact, never a half-written file.
        std::fs::rename(&tmp_path, &self.log_path)?;
        *log_file = OpenOptions::new().create(true).append(true).open(&self.log_path)?;
        Ok(())
    }
}

/// Smallest string greater than every string with byte-prefix `prefix`,
/// found by incrementing the last byte that isn't already 0xff (dropping
/// any trailing 0xff bytes first, since they can't be incremented in
/// place). `None` only if every byte is 0xff.
#[cfg(test)]
mod tests {
    use super::*;

    fn sample(bucket: &str, key: &str) -> LocationRecord {
        LocationRecord {
            bucket: bucket.to_string(),
            key: key.to_string(),
            shard_peers: vec!["http://node0:9710".into(), "http://node1:9710".into()],
            k: 1,
            m: 1,
            original_len: 42,
            retention_mode: None,
            retain_until: None,
            legal_hold: false,
        }
    }

    #[test]
    fn put_then_get_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let store = BitcaskLocationStore::open(dir.path().join("loc.log")).unwrap();
        store.put(sample("b1", "k1")).unwrap();
        let got = store.get("b1", "k1").unwrap();
        assert_eq!(got, sample("b1", "k1"));
    }

    #[test]
    fn restart_rebuilds_index_from_log_replay() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("loc.log");
        {
            let store = BitcaskLocationStore::open(&log_path).unwrap();
            store.put(sample("b1", "k1")).unwrap();
            store.put(sample("b1", "k2")).unwrap();
        }
        // Simulate a process restart: reopen from the same log path.
        let reopened = BitcaskLocationStore::open(&log_path).unwrap();
        assert_eq!(reopened.get("b1", "k1").unwrap(), sample("b1", "k1"));
        assert_eq!(reopened.get("b1", "k2").unwrap(), sample("b1", "k2"));
    }

    #[test]
    fn delete_writes_a_tombstone_that_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("loc.log");
        {
            let store = BitcaskLocationStore::open(&log_path).unwrap();
            store.put(sample("b1", "k1")).unwrap();
            store.delete("b1", "k1").unwrap();
            assert!(store.get("b1", "k1").is_none());
        }
        let reopened = BitcaskLocationStore::open(&log_path).unwrap();
        assert!(reopened.get("b1", "k1").is_none());
    }

    #[test]
    fn retention_fields_round_trip_on_the_same_record() {
        let dir = tempfile::tempdir().unwrap();
        let store = BitcaskLocationStore::open(dir.path().join("loc.log")).unwrap();
        let mut rec = sample("b1", "locked-key");
        rec.retention_mode = Some("COMPLIANCE".to_string());
        rec.retain_until = Some("2030-01-01T00:00:00Z".to_string());
        rec.legal_hold = true;
        store.put(rec.clone()).unwrap();
        assert_eq!(store.get("b1", "locked-key").unwrap(), rec);
    }

    #[test]
    fn list_returns_only_keys_matching_the_prefix_in_this_bucket() {
        let dir = tempfile::tempdir().unwrap();
        let store = BitcaskLocationStore::open(dir.path().join("loc.log")).unwrap();
        store.put(sample("b1", "reports/jan.csv")).unwrap();
        store.put(sample("b1", "reports/feb.csv")).unwrap();
        store.put(sample("b1", "images/a.png")).unwrap();
        // A different bucket's key that would sort right in the middle of
        // b1's "reports/" keys lexicographically, to prove the range scan
        // doesn't spill across bucket boundaries.
        store.put(sample("b1a", "reports/intruder.csv")).unwrap();

        let mut got = store.list("b1", "reports/");
        got.sort();
        assert_eq!(got, vec![("reports/feb.csv".to_string(), 42), ("reports/jan.csv".to_string(), 42)]);
    }

    #[test]
    fn compact_preserves_live_data_and_drops_dead_entries_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("loc.log");
        let store = BitcaskLocationStore::open(&log_path).unwrap();

        store.put(sample("b1", "k1")).unwrap();
        store.put(sample("b1", "k2")).unwrap();
        store.delete("b1", "k1").unwrap();
        // Overwrite k2 a few times -- each one is another line in the
        // uncompacted log for the same still-live key.
        for _ in 0..5 {
            store.put(sample("b1", "k2")).unwrap();
        }
        let size_before = std::fs::metadata(&log_path).unwrap().len();

        store.compact().unwrap();
        let size_after = std::fs::metadata(&log_path).unwrap().len();
        assert!(
            size_after < size_before,
            "compaction should shrink the log: before={size_before} after={size_after}"
        );

        // Still correct in-process...
        assert!(store.get("b1", "k1").is_none());
        assert_eq!(store.get("b1", "k2").unwrap(), sample("b1", "k2"));

        // ...and still correct after a reopen, proving the compacted log
        // on disk is itself a complete, valid replacement, not just an
        // in-memory optimization.
        let reopened = BitcaskLocationStore::open(&log_path).unwrap();
        assert!(reopened.get("b1", "k1").is_none());
        assert_eq!(reopened.get("b1", "k2").unwrap(), sample("b1", "k2"));
    }

    #[test]
    fn a_write_after_compaction_still_works() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("loc.log");
        let store = BitcaskLocationStore::open(&log_path).unwrap();
        store.put(sample("b1", "k1")).unwrap();
        store.compact().unwrap();
        // compact() reopens log_file for appending afterward -- confirm
        // that handle is actually live, not left pointing at a file
        // descriptor orphaned by the rename.
        store.put(sample("b1", "k2")).unwrap();
        assert_eq!(store.get("b1", "k2").unwrap(), sample("b1", "k2"));
        let reopened = BitcaskLocationStore::open(&log_path).unwrap();
        assert_eq!(reopened.get("b1", "k1").unwrap(), sample("b1", "k1"));
        assert_eq!(reopened.get("b1", "k2").unwrap(), sample("b1", "k2"));
    }
}
