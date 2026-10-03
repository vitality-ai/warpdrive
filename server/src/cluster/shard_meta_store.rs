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
    /// Removes a shard key's entry, so a later `get` for it returns `None`.
    /// This bounds *this node's own metadata log* only — the underlying
    /// `(offset, size)` byte range in the shared `__cluster__/{bucket}.bin`
    /// data file is not reclaimed (no `fallocate` hole-punch), matching
    /// `Storage::delete`'s existing queue-don't-reclaim semantics one layer
    /// down. Also, this only ever removes *this node's* entry for a shard
    /// key; it is not a peer-to-peer RPC, so a caller that wants a shard
    /// deleted everywhere it was placed still has to call this on every
    /// peer that holds it (not wired up yet — see #163's follow-up note).
    fn delete(&self, shard_key: &str) -> io::Result<()>;
    /// See `LocationStore::compact` — same reasoning and same mechanism
    /// (#163): rewrite only the live entries to a fresh file, then atomic
    /// rename over the log, so dead (overwritten or deleted) entries stop
    /// costing replay time and disk space on every restart.
    fn compact(&self) -> io::Result<()>;
}

/// Append-only log of JSON-lines `(shard_key, Option<(offset, size)>)`
/// pairs; `None` is a tombstone (delete). In-memory index rebuilt by
/// replaying the log on startup.
///
/// Previously raw CSV (`"{shard_key},{offset},{size}"`, split on `,`
/// with no escaping). Since `shard_key` embeds the user's bucket and
/// object key verbatim, a key containing a literal `,` or `\n` corrupted
/// the log: on the next restart, `open()` either misparsed the line into
/// the wrong fields (an `offset`/`size` that fails to parse as a number)
/// or produced an extra line, and since `open()`'s only caller is a
/// `lazy_static` that `.expect()`s the result, that turned into a panic
/// on the first shard read/write after restart -- the node could no
/// longer serve or store any shard at all (#152). JSON-lines, the same
/// format `location_store.rs` already uses, escapes arbitrary byte
/// content in `shard_key` as a matter of course.
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
            let lines: Vec<String> = BufReader::new(f).lines().collect::<io::Result<_>>()?;
            let last_idx = lines.len().saturating_sub(1);
            for (i, line) in lines.iter().enumerate() {
                if line.is_empty() {
                    continue;
                }
                match serde_json::from_str::<(String, Option<(u64, u64)>)>(line) {
                    Ok((key, Some(record))) => {
                        index.insert(key, record);
                    }
                    Ok((key, None)) => {
                        index.remove(&key);
                    }
                    // Only the *last* line can be a torn write from a crash
                    // mid-append (appends are sequential, so nothing before
                    // it can be torn this way) -- quarantine just that one
                    // line with a warning instead of refusing to start, per
                    // #152's suggested fix. A malformed line anywhere else
                    // is real corruption, not a crash artifact, and still a
                    // hard error.
                    Err(e) if i == last_idx => {
                        log::warn!(
                            "shard meta log {}: ignoring malformed trailing line, \
                             likely a torn write from a crash: {e}",
                            log_path.display()
                        );
                    }
                    Err(e) => return Err(io::Error::new(io::ErrorKind::InvalidData, e)),
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
}

impl ShardMetaStore for BitcaskShardMetaStore {
    fn put(&self, shard_key: &str, offset: u64, size: u64) -> io::Result<()> {
        let line = serde_json::to_string(&(shard_key, Some((offset, size))))
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        {
            let mut f = self.log_file.write().unwrap();
            writeln!(f, "{line}")?;
            f.flush()?;
        }
        self.index.write().unwrap().insert(shard_key.to_string(), (offset, size));
        Ok(())
    }

    fn get(&self, shard_key: &str) -> Option<(u64, u64)> {
        self.index.read().unwrap().get(shard_key).copied()
    }

    fn delete(&self, shard_key: &str) -> io::Result<()> {
        let line = serde_json::to_string(&(shard_key, Option::<(u64, u64)>::None))
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        {
            let mut f = self.log_file.write().unwrap();
            writeln!(f, "{line}")?;
            f.flush()?;
        }
        self.index.write().unwrap().remove(shard_key);
        Ok(())
    }

    fn compact(&self) -> io::Result<()> {
        let index = self.index.read().unwrap();
        let mut log_file = self.log_file.write().unwrap();
        let tmp_path = self.log_path.with_extension("compact.tmp");
        {
            let mut tmp = File::create(&tmp_path)?;
            for (key, &(offset, size)) in index.iter() {
                let line = serde_json::to_string(&(key, Some((offset, size))))
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                writeln!(tmp, "{line}")?;
            }
            tmp.flush()?;
        }
        std::fs::rename(&tmp_path, &self.log_path)?;
        *log_file = OpenOptions::new().create(true).append(true).open(&self.log_path)?;
        Ok(())
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

    /// #152's exact repro: a shard key containing `,` and `\n` (both would
    /// have broken the old raw-CSV log) round-trips correctly, including
    /// across a restart that has to replay the log.
    #[test]
    fn a_key_containing_commas_and_newlines_round_trips_across_restart() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("shardmeta.log");
        let tricky_key = "bucket:0:a,b\nc:shard0";
        {
            let store = BitcaskShardMetaStore::open(&log_path).unwrap();
            store.put(tricky_key, 7, 42).unwrap();
            assert_eq!(store.get(tricky_key), Some((7, 42)));
        }
        let reopened = BitcaskShardMetaStore::open(&log_path).unwrap();
        assert_eq!(reopened.get(tricky_key), Some((7, 42)));
    }

    /// #152's other suggested fix: a torn last line (process crashed
    /// mid-`writeln!`, or mid-`flush`) shouldn't take down the whole
    /// store on the next restart -- only that one line is lost.
    #[test]
    fn a_malformed_trailing_line_is_quarantined_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("shardmeta.log");
        {
            let store = BitcaskShardMetaStore::open(&log_path).unwrap();
            store.put("k1", 0, 100).unwrap();
        }
        {
            let mut f = OpenOptions::new().append(true).open(&log_path).unwrap();
            // A JSON line with no closing bracket -- exactly what a crash
            // mid-`writeln!` would leave behind.
            writeln!(f, "[\"k2\",[500,30").unwrap();
        }
        let reopened = BitcaskShardMetaStore::open(&log_path).unwrap();
        assert_eq!(reopened.get("k1"), Some((0, 100)));
        assert_eq!(reopened.get("k2"), None);
    }

    /// The quarantine above is specifically for a *trailing* line -- a
    /// malformed line anywhere earlier in the file is real corruption
    /// (nothing before the last line can be torn by a crash, since
    /// appends are sequential) and still has to be a hard error, not
    /// silently dropped.
    #[test]
    fn a_malformed_line_before_the_last_one_is_still_a_hard_error() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("shardmeta.log");
        {
            let mut f = OpenOptions::new().create(true).append(true).open(&log_path).unwrap();
            writeln!(f, "not valid json at all").unwrap();
            writeln!(f, "{}", serde_json::to_string(&("k1", Some((0u64, 100u64)))).unwrap()).unwrap();
        }
        assert!(BitcaskShardMetaStore::open(&log_path).is_err());
    }

    #[test]
    fn overwrite_updates_the_mapping() {
        let dir = tempfile::tempdir().unwrap();
        let store = BitcaskShardMetaStore::open(dir.path().join("shardmeta.log")).unwrap();
        store.put("k1", 0, 100).unwrap();
        store.put("k1", 500, 300).unwrap();
        assert_eq!(store.get("k1"), Some((500, 300)));
    }

    #[test]
    fn delete_removes_the_mapping_and_survives_replay() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("shardmeta.log");
        {
            let store = BitcaskShardMetaStore::open(&log_path).unwrap();
            store.put("k1", 0, 100).unwrap();
            store.delete("k1").unwrap();
            assert_eq!(store.get("k1"), None);
        }
        let reopened = BitcaskShardMetaStore::open(&log_path).unwrap();
        assert_eq!(reopened.get("k1"), None);
    }

    #[test]
    fn compact_preserves_live_entries_and_drops_dead_ones_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("shardmeta.log");
        let store = BitcaskShardMetaStore::open(&log_path).unwrap();
        store.put("keep", 0, 100).unwrap();
        for i in 0..20 {
            store.put(&format!("churn{i}"), i, 10).unwrap();
            store.delete(&format!("churn{i}")).unwrap();
        }
        let size_before = std::fs::metadata(&log_path).unwrap().len();

        store.compact().unwrap();

        let size_after = std::fs::metadata(&log_path).unwrap().len();
        assert!(size_after < size_before, "compact should shrink the log file");
        assert_eq!(store.get("keep"), Some((0, 100)));
        for i in 0..20 {
            assert_eq!(store.get(&format!("churn{i}")), None);
        }

        let reopened = BitcaskShardMetaStore::open(&log_path).unwrap();
        assert_eq!(reopened.get("keep"), Some((0, 100)));
    }

    #[test]
    fn a_put_after_compaction_still_works() {
        let dir = tempfile::tempdir().unwrap();
        let store = BitcaskShardMetaStore::open(dir.path().join("shardmeta.log")).unwrap();
        store.put("k1", 0, 100).unwrap();
        store.compact().unwrap();
        store.put("k2", 100, 200).unwrap();
        assert_eq!(store.get("k1"), Some((0, 100)));
        assert_eq!(store.get("k2"), Some((100, 200)));
    }
}
