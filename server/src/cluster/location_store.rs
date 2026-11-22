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
use std::collections::HashMap;
use std::io;
use std::path::Path;
use warp_meta_store::{Record, WarpMetaStore};
pub use warp_meta_store::{Applied, Entry};

/// Length-prefixes `user_id` and `bucket` so their exact boundaries are
/// always recoverable regardless of what bytes any of the three fields
/// contain (#150). `key` is last, so it doesn't need its own prefix.
pub fn record_key(user_id: &str, bucket: &str, key: &str) -> String {
    format!("{}:{}:{}:{}:{}", user_id.len(), user_id, bucket.len(), bucket, key)
}
use warp_meta_store::BitcaskEngine;

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

/// The S3 metadata a PUT wants set on the record it's about to create --
/// bundled into one value instead of a 9-argument function signature,
/// shared between `LocationRecord` and `ContentDependentRecord` since
/// both carry the identical set of fields (see each one's own doc for why
/// they're still two separate record *shapes* overall). Built by a
/// caller that already has the request headers and computed etag (Phase
/// 3's auth-aware `/cluster/s3/...` handlers); the bare, pre-auth
/// `/cluster/{bucket}/{key}` routes pass `Default::default()`, preserving
/// their exact pre-this-change behavior (every field empty/`None`).
#[derive(Debug, Clone, Default)]
pub struct ObjectS3Metadata {
    pub etag: Option<String>,
    pub content_type: Option<String>,
    pub last_modified: Option<String>,
    pub user_metadata: HashMap<String, String>,
    pub cache_control: Option<String>,
    pub expires: Option<String>,
    pub content_encoding: Option<String>,
    pub checksum_algorithm: Option<String>,
    pub checksum_value: Option<String>,
    pub checksum_type: Option<String>,
}

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
    /// Order of this write among all writes to the key (`hlc`). A node
    /// keeps whichever write has the highest `seq`, so every node ends on
    /// the same record whatever order writes arrive in (RFC 6).
    #[serde(default)]
    pub seq: u64,
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
    /// S3 object tags (`PutObjectTagging`/`GetObjectTagging`), same shape
    /// as single-node's tag storage. `#[serde(default)]`: empty for a
    /// record written before this field existed.
    #[serde(default)]
    pub tags: Vec<(String, String)>,
    /// A small object's bytes, base64, stored in the record itself instead
    /// of as erasure-coded shards (`WARPDRIVE_INLINE_MAX_BYTES`, RFC 5). The
    /// record is replicated to every node with the same write quorum, so a
    /// PUT is one round and a GET is served from the receiving node's own
    /// copy. `None` for an object stored as shards; `shard_peers` is then
    /// empty.
    #[serde(default)]
    pub inline_data: Option<String>,
}

pub trait LocationStore: Send + Sync {
    /// A new version of the object (PUT). Applied only if `record.seq` is
    /// newer than the key's current state (see `record_log`).
    fn put(&self, record: LocationRecord) -> io::Result<Applied<LocationRecord>>;
    /// A metadata change (tags, retention) to the version `record` names.
    /// Refused if that version is no longer the live one.
    fn update(&self, record: LocationRecord) -> io::Result<Applied<LocationRecord>>;
    fn get(&self, user_id: &str, bucket: &str, key: &str) -> Option<LocationRecord>;
    /// The key's state, tombstone included.
    fn entry(&self, user_id: &str, bucket: &str, key: &str) -> Option<Entry<LocationRecord>>;
    /// Deletes the key at `seq`, leaving a tombstone.
    fn delete(&self, user_id: &str, bucket: &str, key: &str, seq: u64) -> io::Result<Applied<LocationRecord>>;
    /// The live record in `bucket` with the greatest key at most `key`.
    fn floor(&self, user_id: &str, bucket: &str, key: &str) -> Option<(String, LocationRecord)>;
    /// Keys (with their byte size) in `bucket` whose key starts with
    /// `prefix`, for ListObjects.
    fn list(&self, user_id: &str, bucket: &str, prefix: &str) -> Vec<(String, u64)>;
    /// Rewrites the log to hold one line per key (#163), so its size and
    /// the next restart's replay time follow the number of keys, not the
    /// number of writes ever made.
    fn compact(&self) -> io::Result<()>;
}

impl Record for LocationRecord {
    fn seq(&self) -> u64 {
        self.seq
    }
    fn version(&self) -> &str {
        &self.version
    }
}

/// `LocationStore` on ordered records (`ordered`) over a per-node `Engine`.
/// The default engine keeps a `BTreeMap` index: `record_key` groups each
/// bucket's keys contiguously, so a prefix listing is a bounded range scan.
pub struct BitcaskLocationStore<E = BitcaskEngine<Entry<LocationRecord>>> {
    log: WarpMetaStore<LocationRecord, E>,
}

impl BitcaskLocationStore {
    pub fn open(log_path: impl AsRef<Path>) -> io::Result<Self> {
        Ok(Self { log: WarpMetaStore::new(BitcaskEngine::open(log_path, "location")?) })
    }

    pub fn path(&self) -> &Path {
        self.log.engine().path()
    }
}

impl<E: warp_meta_store::Engine<Entry<LocationRecord>>> BitcaskLocationStore<E> {
    /// The store on any engine.
    pub fn with_engine(engine: E) -> Self {
        Self { log: WarpMetaStore::new(engine) }
    }
}

impl<E: warp_meta_store::Engine<Entry<LocationRecord>>> LocationStore for BitcaskLocationStore<E> {
    fn put(&self, record: LocationRecord) -> io::Result<Applied<LocationRecord>> {
        self.log.put(record_key(&record.user_id, &record.bucket, &record.key), record)
    }

    fn update(&self, record: LocationRecord) -> io::Result<Applied<LocationRecord>> {
        self.log.update(record_key(&record.user_id, &record.bucket, &record.key), record)
    }

    fn get(&self, user_id: &str, bucket: &str, key: &str) -> Option<LocationRecord> {
        self.log.get(&record_key(user_id, bucket, key))
    }

    fn entry(&self, user_id: &str, bucket: &str, key: &str) -> Option<Entry<LocationRecord>> {
        self.log.entry(&record_key(user_id, bucket, key))
    }

    fn delete(&self, user_id: &str, bucket: &str, key: &str, seq: u64) -> io::Result<Applied<LocationRecord>> {
        self.log.delete(record_key(user_id, bucket, key), seq)
    }

    fn floor(&self, user_id: &str, bucket: &str, key: &str) -> Option<(String, LocationRecord)> {
        let bucket_prefix = format!("{}:{}:{}:{}:", user_id.len(), user_id, bucket.len(), bucket);
        self.log
            .floor_live(&bucket_prefix, &record_key(user_id, bucket, key))
            .map(|(k, r)| (k[bucket_prefix.len()..].to_string(), r))
    }

    fn list(&self, user_id: &str, bucket: &str, prefix: &str) -> Vec<(String, u64)> {
        let bucket_prefix = format!("{}:{}:{}:{}:", user_id.len(), user_id, bucket.len(), bucket);
        let full_prefix = format!("{bucket_prefix}{prefix}");
        self.log.list_live(&full_prefix, |k, r| (k[bucket_prefix.len()..].to_string(), r.original_len as u64))
    }

    fn compact(&self) -> io::Result<()> {
        self.log.compact()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::io::Write;

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
            seq: 1,
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
            tags: Vec::new(),
            inline_data: None,
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
    fn inline_object_bytes_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("loc.log");
        let mut rec = sample_for("alice", "logs", "small");
        rec.shard_peers = Vec::new();
        rec.inline_data = Some("aGVsbG8=".into());
        BitcaskLocationStore::open(&path).unwrap().put(rec.clone()).unwrap();

        let reopened = BitcaskLocationStore::open(&path).unwrap();
        assert_eq!(reopened.get("alice", "logs", "small").unwrap(), rec);
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

        store.delete("alice", "logs", "k1", 2).unwrap();
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
            store.delete("u1", "b1", "k1", 2).unwrap();
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
        store.delete("u1", "b1", "k1", 2).unwrap();
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
}
