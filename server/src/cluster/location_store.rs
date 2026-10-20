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
use std::collections::{BTreeMap, HashMap};
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

/// Sentinel `user_id` for every request that arrives with no authenticated
/// identity -- which today is *every* request on the bare, pre-Phase-3
/// `/cluster/{bucket}/{key}` routes (`authenticate_s3_request` is never
/// called from them). An empty string can never collide with a real S3
/// access-key-style user id, so this namespace stays cleanly separate from
/// whatever real per-tenant user ids Phase 3's shared-handler routes start
/// supplying. `#[serde(default)]` on `LocationRecord::user_id` means every
/// record written before this field existed also reads back as this same
/// sentinel, which is correct: that data *was* written with no tenant
/// concept at all.
pub const UNAUTHENTICATED_USER: &str = "";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LocationRecord {
    /// See `UNAUTHENTICATED_USER`. `#[serde(default)]`: empty for a record
    /// written before this field existed.
    #[serde(default)]
    pub user_id: String,
    pub bucket: String,
    pub key: String,
    /// Index i = the peer holding shard i (data shards first, then parity).
    pub shard_peers: Vec<String>,
    pub k: usize,
    pub m: usize,
    pub original_len: usize,
    /// This version's shards live at `shard_storage::versioned_key(key,
    /// version)`, never plain `key` -- so an overwrite's shard writes can
    /// never land on the same shard key an in-flight or still-pinned read
    /// of the *previous* version is using, however that write turns out
    /// (full success, partial failure, or racing against another PUT of
    /// the same key). The pin only ever points at one version's worth of
    /// shards at a time, flipped to the new version's id after quorum
    /// (#151). `#[serde(default)]`: empty string means a record written
    /// before this field existed, whose shards are at the old, unversioned
    /// `shard_key(bucket, key, idx)` -- see callers of this field.
    #[serde(default)]
    pub version: String,
    pub retention_mode: Option<String>, // "GOVERNANCE" | "COMPLIANCE"
    pub retain_until: Option<String>,   // RFC3339
    pub legal_hold: bool,
    /// MD5 ETag in double quotes, same shape as single-node's
    /// `metadata::Metadata::etag`. `#[serde(default)]`: `None` for a record
    /// written before this field existed.
    #[serde(default)]
    pub etag: Option<String>,
    /// MIME type supplied on PUT via `Content-Type`.
    #[serde(default)]
    pub content_type: Option<String>,
    /// RFC 2616 date string set on PUT -- same format single-node's
    /// `common.rs::last_modified_now` produces.
    #[serde(default)]
    pub last_modified: Option<String>,
    /// `x-amz-meta-*` headers, header name without the prefix.
    #[serde(default)]
    pub user_metadata: HashMap<String, String>,
    #[serde(default)]
    pub cache_control: Option<String>,
    #[serde(default)]
    pub expires: Option<String>,
    #[serde(default)]
    pub content_encoding: Option<String>,
    #[serde(default)]
    pub checksum_algorithm: Option<String>,
    #[serde(default)]
    pub checksum_value: Option<String>,
    #[serde(default)]
    pub checksum_type: Option<String>,
}

pub trait LocationStore: Send + Sync {
    fn put(&self, record: LocationRecord) -> io::Result<()>;
    fn get(&self, user_id: &str, bucket: &str, key: &str) -> Option<LocationRecord>;
    fn delete(&self, user_id: &str, bucket: &str, key: &str) -> io::Result<()>;
    /// Keys (with their byte size) in `bucket` whose key starts with
    /// `prefix` — the minimal S3 ListObjectsV2 surface needs this (object
    /// stores speaking the real S3 protocol, like Lance's client, list a
    /// dataset's files; the raw `/cluster/{bucket}/{key}` API never needed
    /// this since callers always know their own key).
    fn list(&self, user_id: &str, bucket: &str, prefix: &str) -> Vec<(String, u64)>;
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

/// Length-prefixes `user_id` and `bucket` so their exact boundaries are
/// always recoverable regardless of what bytes any of the three fields
/// contain -- `key` is last, so it doesn't need its own length prefix,
/// it's simply "whatever comes after bucket's prefix." Previously (#150,
/// before `user_id` existed) `format!("{bucket}/{key}")`: bucket `a/b` +
/// key `c` and bucket `a` + key `b/c` both produced `a/b/c`. Adding
/// `user_id` the same unprefixed way would reopen that exact bug one level
/// up (two tenants' bucket/key pairs colliding across a `user_id` boundary
/// instead of a bucket boundary), so it gets the same length-prefix
/// treatment, not a bare separator.
fn record_key(user_id: &str, bucket: &str, key: &str) -> String {
    format!("{}:{}:{}:{}:{}", user_id.len(), user_id, bucket.len(), bucket, key)
}

/// Append-only log of JSON-lines `(record_key, Option<LocationRecord>)`
/// pairs; `None` is a tombstone (delete). In-memory index is rebuilt by
/// replaying the whole log on startup. `BTreeMap`, not `HashMap` (#163):
/// `record_key` leads with bucket's own length-prefix, so lexicographic
/// order still groups every bucket's keys contiguously (see `record_key`'s
/// own doc for why this is also collision-free, unlike a plain `/`
/// separator), which is what makes `list`'s prefix lookup a bounded range
/// scan instead of a full-table scan.
pub struct BitcaskLocationStore {
    log_path: PathBuf,
    log_file: RwLock<File>,
    index: RwLock<BTreeMap<String, LocationRecord>>,
    /// #155: `log_file` and `index` are two *separate* locks, so without
    /// this, two concurrent writers (e.g. a replicated PUT and a
    /// replicated DELETE for the same key, arriving together) could each
    /// finish their own log-append-then-index-update in a different
    /// order, leaving the log and the index permanently disagreeing about
    /// which one happened last -- invisible until the next restart
    /// replays the log and gets the *other* answer (a deleted object
    /// "resurrects", or vice versa). Held across the full append-then-
    /// update sequence in every mutating method (`put`, `delete`,
    /// `compact`), so the two data structures can never apply writes in
    /// different orders from each other.
    write_lock: std::sync::Mutex<()>,
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
            let lines: Vec<String> = BufReader::new(f).lines().collect::<io::Result<_>>()?;
            let last_idx = lines.len().saturating_sub(1);
            for (i, line) in lines.iter().enumerate() {
                if line.is_empty() {
                    continue;
                }
                match serde_json::from_str::<(String, Option<LocationRecord>)>(line) {
                    Ok((rk, Some(r))) => {
                        index.insert(rk, r);
                    }
                    Ok((rk, None)) => {
                        index.remove(&rk);
                    }
                    // Same reasoning as `ShardMetaStore::open` (#152): only
                    // the last line can be a torn write from a crash
                    // mid-append, so quarantine just that one with a
                    // warning instead of refusing to start.
                    Err(e) if i == last_idx => {
                        log::warn!(
                            "location log {}: ignoring malformed trailing line, \
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
            write_lock: std::sync::Mutex::new(()),
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
        let rk = record_key(&record.user_id, &record.bucket, &record.key);
        let line = serde_json::to_string(&(rk.clone(), Some(record.clone())))
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        // #155: held across both the append and the index update, so a
        // concurrent put/delete for the same key can never apply to the
        // log and the index in different orders.
        let _write_guard = self.write_lock.lock().unwrap();
        self.append_line(&line)?;
        self.index.write().unwrap().insert(rk, record);
        Ok(())
    }

    fn get(&self, user_id: &str, bucket: &str, key: &str) -> Option<LocationRecord> {
        self.index.read().unwrap().get(&record_key(user_id, bucket, key)).cloned()
    }

    fn delete(&self, user_id: &str, bucket: &str, key: &str) -> io::Result<()> {
        let rk = record_key(user_id, bucket, key);
        let line = serde_json::to_string(&(rk.clone(), Option::<LocationRecord>::None))
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let _write_guard = self.write_lock.lock().unwrap();
        self.append_line(&line)?;
        self.index.write().unwrap().remove(&rk);
        Ok(())
    }

    fn list(&self, user_id: &str, bucket: &str, prefix: &str) -> Vec<(String, u64)> {
        let bucket_prefix = format!("{}:{}:{}:{}:", user_id.len(), user_id, bucket.len(), bucket);
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
        // #155: same `write_lock` as `put`/`delete`, so compaction can't
        // interleave with either -- a concurrent put/delete must see
        // either the pre- or post-compaction state, never a log file
        // mid-rewrite out from under it, and never a snapshot of the
        // index that's already out of sync with what made it into the
        // compacted file.
        let _write_guard = self.write_lock.lock().unwrap();
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
        sample_for("u1", bucket, key)
    }

    fn sample_for(user_id: &str, bucket: &str, key: &str) -> LocationRecord {
        LocationRecord {
            user_id: user_id.to_string(),
            bucket: bucket.to_string(),
            key: key.to_string(),
            shard_peers: vec!["http://node0:9710".into(), "http://node1:9710".into()],
            k: 1,
            m: 1,
            original_len: 42,
            version: "v1".to_string(),
            retention_mode: None,
            retain_until: None,
            legal_hold: false,
            etag: None,
            content_type: None,
            last_modified: None,
            user_metadata: HashMap::new(),
            cache_control: None,
            expires: None,
            content_encoding: None,
            checksum_algorithm: None,
            checksum_value: None,
            checksum_type: None,
        }
    }

    /// #150's exact repro, applied to this store's own index: bucket
    /// `a/b` + key `c` must not collide with bucket `a` + key `b/c`.
    #[test]
    fn record_key_does_not_collide_when_a_separator_moves_across_the_bucket_key_boundary() {
        assert_ne!(record_key("u1", "a/b", "c"), record_key("u1", "a", "b/c"));
    }

    /// The tenant-isolation fix's core invariant: two different users
    /// writing the exact same `(bucket, key)` must land in two distinct
    /// records, not collide onto one -- see `record_key`'s own doc.
    #[test]
    fn record_key_does_not_collide_across_different_users_for_the_same_bucket_and_key() {
        assert_ne!(record_key("alice", "shared-bucket", "k"), record_key("bob", "shared-bucket", "k"));
    }

    #[test]
    fn two_users_writing_the_same_bucket_and_key_get_independent_records() {
        let dir = tempfile::tempdir().unwrap();
        let store = BitcaskLocationStore::open(dir.path().join("loc.log")).unwrap();

        let mut alice_rec = sample_for("alice", "logs", "k1");
        alice_rec.original_len = 111;
        let mut bob_rec = sample_for("bob", "logs", "k1");
        bob_rec.original_len = 222;

        store.put(alice_rec.clone()).unwrap();
        store.put(bob_rec.clone()).unwrap();

        assert_eq!(store.get("alice", "logs", "k1").unwrap(), alice_rec);
        assert_eq!(store.get("bob", "logs", "k1").unwrap(), bob_rec);

        store.delete("alice", "logs", "k1").unwrap();
        assert!(store.get("alice", "logs", "k1").is_none());
        assert_eq!(store.get("bob", "logs", "k1").unwrap(), bob_rec, "deleting alice's record must not touch bob's");
    }

    #[test]
    fn put_then_get_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let store = BitcaskLocationStore::open(dir.path().join("loc.log")).unwrap();
        store.put(sample("b1", "k1")).unwrap();
        let got = store.get("u1", "b1", "k1").unwrap();
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
        assert_eq!(reopened.get("u1", "b1", "k1").unwrap(), sample("b1", "k1"));
        assert_eq!(reopened.get("u1", "b1", "k2").unwrap(), sample("b1", "k2"));
    }

    #[test]
    fn delete_writes_a_tombstone_that_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("loc.log");
        {
            let store = BitcaskLocationStore::open(&log_path).unwrap();
            store.put(sample("b1", "k1")).unwrap();
            store.delete("u1", "b1", "k1").unwrap();
            assert!(store.get("u1", "b1", "k1").is_none());
        }
        let reopened = BitcaskLocationStore::open(&log_path).unwrap();
        assert!(reopened.get("u1", "b1", "k1").is_none());
    }

    /// Same reasoning as `shard_meta_store.rs`'s matching test (#152): a
    /// torn last line (crash mid-`writeln!`) shouldn't take the whole
    /// store down on the next restart.
    #[test]
    fn a_malformed_trailing_line_is_quarantined_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("loc.log");
        {
            let store = BitcaskLocationStore::open(&log_path).unwrap();
            store.put(sample("b1", "k1")).unwrap();
        }
        {
            let mut f = OpenOptions::new().append(true).open(&log_path).unwrap();
            writeln!(f, "[\"b1:k2\",{{\"bucket\":\"b1\"").unwrap();
        }
        let reopened = BitcaskLocationStore::open(&log_path).unwrap();
        assert_eq!(reopened.get("u1", "b1", "k1").unwrap(), sample("b1", "k1"));
        assert!(reopened.get("u1", "b1", "k2").is_none());
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
        assert_eq!(store.get("u1", "b1", "locked-key").unwrap(), rec);
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

        let mut got = store.list("u1", "b1", "reports/");
        got.sort();
        assert_eq!(got, vec![("reports/feb.csv".to_string(), 42), ("reports/jan.csv".to_string(), 42)]);
    }

    #[test]
    fn list_does_not_spill_across_a_user_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let store = BitcaskLocationStore::open(dir.path().join("loc.log")).unwrap();
        store.put(sample_for("alice", "shared", "k1")).unwrap();
        store.put(sample_for("bob", "shared", "k1")).unwrap();

        assert_eq!(store.list("alice", "shared", "").len(), 1);
        assert_eq!(store.list("bob", "shared", "").len(), 1);
    }

    #[test]
    fn compact_preserves_live_data_and_drops_dead_entries_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("loc.log");
        let store = BitcaskLocationStore::open(&log_path).unwrap();

        store.put(sample("b1", "k1")).unwrap();
        store.put(sample("b1", "k2")).unwrap();
        store.delete("u1", "b1", "k1").unwrap();
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
        assert!(store.get("u1", "b1", "k1").is_none());
        assert_eq!(store.get("u1", "b1", "k2").unwrap(), sample("b1", "k2"));

        // ...and still correct after a reopen, proving the compacted log
        // on disk is itself a complete, valid replacement, not just an
        // in-memory optimization.
        let reopened = BitcaskLocationStore::open(&log_path).unwrap();
        assert!(reopened.get("u1", "b1", "k1").is_none());
        assert_eq!(reopened.get("u1", "b1", "k2").unwrap(), sample("b1", "k2"));
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
        assert_eq!(store.get("u1", "b1", "k2").unwrap(), sample("b1", "k2"));
        let reopened = BitcaskLocationStore::open(&log_path).unwrap();
        assert_eq!(reopened.get("u1", "b1", "k1").unwrap(), sample("b1", "k1"));
        assert_eq!(reopened.get("u1", "b1", "k2").unwrap(), sample("b1", "k2"));
    }

    /// #155's core invariant: the in-memory index and the on-disk log can
    /// never disagree about a key's final state, no matter how many
    /// threads raced to put/delete it concurrently. Before the fix (two
    /// separate locks, log appended then index updated with no shared
    /// lock across the two), a thread's own append could land before or
    /// after another thread's index update in either order, so replaying
    /// the log after a restart could produce a different answer than the
    /// live index had -- a deleted object "resurrecting", or vice versa.
    /// With a single lock held across both steps, every operation's
    /// append-then-update pair is atomic relative to every other one, so
    /// the live index and a fresh replay of the log must always agree,
    /// whatever order the race actually resolved in.
    #[test]
    fn concurrent_put_delete_on_the_same_key_never_diverges_log_from_index() {
        use std::thread;

        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("loc.log");
        let store = std::sync::Arc::new(BitcaskLocationStore::open(&log_path).unwrap());

        let handles: Vec<_> = (0..16)
            .map(|t| {
                let store = std::sync::Arc::clone(&store);
                thread::spawn(move || {
                    for i in 0..50 {
                        if (t + i) % 2 == 0 {
                            store.put(sample("b1", "racykey")).unwrap();
                        } else {
                            let _ = store.delete("u1", "b1", "racykey");
                        }
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let live = store.get("u1", "b1", "racykey");
        let replayed = BitcaskLocationStore::open(&log_path).unwrap().get("u1", "b1", "racykey");
        assert_eq!(live, replayed, "live index and a fresh log replay must agree");
    }
}
