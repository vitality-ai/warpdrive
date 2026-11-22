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
use std::io;
use std::path::Path;
use warp_meta_store::{BitcaskEngine, Engine};

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
    /// Points `shard_key` at `to` only if it still points at `from`, as one
    /// atomic step. The compactor moves a shard this way, so a shard that
    /// was rewritten or deleted meanwhile is never pointed back at stale
    /// bytes. Returns whether it moved.
    fn move_if(&self, shard_key: &str, from: (u64, u64), to: (u64, u64)) -> io::Result<bool>;
    /// Removes `shard_key` only if it still points at `at`. Returns whether
    /// it removed it.
    fn delete_if_at(&self, shard_key: &str, at: (u64, u64)) -> io::Result<bool>;
    /// `delete_if_at` for many shards, durable on return with one sync.
    /// Returns how many it removed.
    fn delete_many_if_at(&self, shards: &[(String, (u64, u64))]) -> io::Result<usize>;
    /// Every shard key with its `(offset, size)`, for the space reclaimer.
    fn entries(&self) -> Vec<(String, (u64, u64))>;
    /// See `LocationStore::compact` — same reasoning and same mechanism
    /// (#163): rewrite only the live entries to a fresh file, then atomic
    /// rename over the log, so dead (overwritten or deleted) entries stop
    /// costing replay time and disk space on every restart.
    fn compact(&self) -> io::Result<()>;
    /// Whether the log has built up enough dead lines to compact (RFC 6
    /// step 2: more than twice its live entries).
    fn compaction_due(&self) -> bool {
        false
    }
}

/// `ShardMetaStore` on a per-node `Engine`. The default engine uses a
/// `HashMap` index: this store only does point lookups, on the shard write
/// and read path. JSON lines (#152): shard keys embed user-supplied bucket
/// and object keys, which may contain any byte.
pub struct BitcaskShardMetaStore<E = BitcaskEngine<(u64, u64), HashMap<String, (u64, u64)>>> {
    engine: E,
}

impl BitcaskShardMetaStore {
    pub fn open(log_path: impl AsRef<Path>) -> io::Result<Self> {
        Ok(Self { engine: BitcaskEngine::open(log_path, "shard meta")? })
    }

    pub fn path(&self) -> &Path {
        self.engine.path()
    }
}

impl<E: Engine<(u64, u64)>> BitcaskShardMetaStore<E> {
    /// The store on any engine.
    pub fn with_engine(engine: E) -> Self {
        Self { engine }
    }
}

impl<E: Engine<(u64, u64)>> ShardMetaStore for BitcaskShardMetaStore<E> {
    fn put(&self, shard_key: &str, offset: u64, size: u64) -> io::Result<()> {
        self.engine.put(shard_key.to_string(), (offset, size))
    }

    fn get(&self, shard_key: &str) -> Option<(u64, u64)> {
        self.engine.get(shard_key)
    }

    fn delete(&self, shard_key: &str) -> io::Result<()> {
        self.engine.delete(shard_key)
    }

    fn move_if(&self, shard_key: &str, from: (u64, u64), to: (u64, u64)) -> io::Result<bool> {
        Ok(self.engine.put_if(shard_key.to_string(), to, |cur| if cur == Some(&from) { Ok(()) } else { Err(()) })?.is_ok())
    }

    fn delete_if_at(&self, shard_key: &str, at: (u64, u64)) -> io::Result<bool> {
        self.engine.delete_if(shard_key, |cur| *cur != at)
    }

    fn delete_many_if_at(&self, shards: &[(String, (u64, u64))]) -> io::Result<usize> {
        let at: HashMap<&str, (u64, u64)> = shards.iter().map(|(k, a)| (k.as_str(), *a)).collect();
        let keys: Vec<String> = shards.iter().map(|(k, _)| k.clone()).collect();
        self.engine.delete_many_if(&keys, |k, cur| at.get(k) != Some(cur))
    }

    fn entries(&self) -> Vec<(String, (u64, u64))> {
        self.engine.snapshot()
    }

    fn compact(&self) -> io::Result<()> {
        self.engine.compact()
    }

    fn compaction_due(&self) -> bool {
        self.engine.compaction_due()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::io::Write;

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
    fn move_if_and_delete_if_at_act_only_on_the_expected_location() {
        let dir = tempfile::tempdir().unwrap();
        let s = BitcaskShardMetaStore::open(dir.path().join("shard_meta.log")).unwrap();
        s.put("k", 10, 4).unwrap();
        assert!(!s.move_if("k", (11, 4), (100, 4)).unwrap(), "stale source: not moved");
        assert!(!s.move_if("gone", (10, 4), (100, 4)).unwrap(), "a missing key is never created");
        assert_eq!(s.get("gone"), None);
        assert!(s.move_if("k", (10, 4), (100, 4)).unwrap());
        assert_eq!(s.get("k"), Some((100, 4)));
        assert!(!s.delete_if_at("k", (10, 4)).unwrap(), "old location: kept");
        assert!(s.delete_if_at("k", (100, 4)).unwrap());
        assert_eq!(BitcaskShardMetaStore::open(dir.path().join("shard_meta.log")).unwrap().get("k"), None);
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

    /// Same invariant and same reasoning as `location_store.rs`'s matching
    /// test (#155): the live index and a fresh replay of the log must
    /// always agree, however a concurrent put/delete race on the same key
    /// actually resolved.
    #[test]
    fn concurrent_put_delete_on_the_same_key_never_diverges_log_from_index() {
        use std::thread;

        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("shardmeta.log");
        let store = std::sync::Arc::new(BitcaskShardMetaStore::open(&log_path).unwrap());

        let handles: Vec<_> = (0..16)
            .map(|t| {
                let store = std::sync::Arc::clone(&store);
                thread::spawn(move || {
                    for i in 0..50 {
                        if (t + i) % 2 == 0 {
                            store.put("racykey", (t * 1000 + i) as u64, 10).unwrap();
                        } else {
                            let _ = store.delete("racykey");
                        }
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let live = store.get("racykey");
        let replayed = BitcaskShardMetaStore::open(&log_path).unwrap().get("racykey");
        assert_eq!(live, replayed, "live index and a fresh log replay must agree");
    }
}
