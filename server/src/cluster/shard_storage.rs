//! Shard storage logic for the gRPC `PeerClient` server side.
//!
//! **Previously** this called through `StorageService`/`MetadataService`,
//! which serializes every metadata call through one process-wide
//! `lazy_static Arc<Mutex<Connection>>` in `metadata/sqlite_store.rs` — the
//! exact single-writer bottleneck the user flagged early in this project,
//! just one layer deeper than `location_store.rs` (which already fixed it
//! for the placement pin). A GCP load test confirmed this mattered:
//! throughput stayed flat going from 64 to 256 concurrent clients on a
//! 24-core VM, evidence of a serialization point, not a CPU limit.
//!
//! **Now** this calls the `Storage` trait directly (unchanged — still the
//! same local-disk `write`/`read`, just not routed through the SQLite
//! metadata layer) and records each shard's `(offset, size)` in
//! `shard_meta_store.rs` (Bitcask, same pattern as `location_store.rs`)
//! instead of SQLite. **The HTTP `PeerClient`'s server side is unaffected**
//! — it's still the existing native `/put/{key}`/`/get/{key}` handlers,
//! unchanged, which still go through `MetadataService`/SQLite. That's a
//! real, now-understood difference between the two transports beyond raw
//! latency: gRPC (the default) avoids the mutex, HTTP (the fallback)
//! doesn't. Documented, not reconciled — reconciling it would mean
//! rewriting the native API's own handlers, out of scope here.

use actix_web::error::{ErrorInternalServerError, ErrorNotFound};
use actix_web::Error;
use lazy_static::lazy_static;
use std::sync::Arc;
use std::time::Instant;

use super::shard_meta_store::{BitcaskShardMetaStore, ShardMetaStore};
use super::timing_stats::Phase;
use crate::storage::config::StorageConfig;
use crate::storage::Storage;

/// Reserved user_id namespace for internal cluster shard storage — keeps
/// shard files physically isolated from any real tenant's own storage
/// directory (user_id is the top-level directory under STORAGE_DIRECTORY).
pub const CLUSTER_SHARD_USER: &str = "__cluster__";

lazy_static! {
    static ref SHARD_META_STORE: Arc<dyn ShardMetaStore> = {
        let log_path = std::env::var("WARPDRIVE_SHARD_META_LOG").unwrap_or_else(|_| "shard_meta.log".to_string());
        Arc::new(BitcaskShardMetaStore::open(&log_path).expect("failed to open shard meta store log"))
    };
}

/// Server-side phase timing for `store_shard`, diagnostic only — added to
/// find out which part of this path is actually slow under concurrent gRPC
/// load, after fixing `local_store.rs`'s write lock didn't move the needle
/// on `shard_fanout` latency at all (see docs/Distributed-Engine-Plan.md).
#[derive(Default)]
pub struct ShardServerTiming {
    pub storage_write: Phase,
    pub meta_put: Phase,
    pub total: Phase,
}

impl ShardServerTiming {
    pub fn summary(&self) -> String {
        format!(
            "n={}\nstorage_write_avg_ms={:.3} storage_write_max_ms={:.3}\nmeta_put_avg_ms={:.3} meta_put_max_ms={:.3}\ntotal_avg_ms={:.3} total_max_ms={:.3}\n",
            self.total.count(),
            self.storage_write.avg_ms(),
            self.storage_write.max_ms(),
            self.meta_put.avg_ms(),
            self.meta_put.max_ms(),
            self.total.avg_ms(),
            self.total.max_ms(),
        )
    }
}

lazy_static! {
    pub static ref SERVER_TIMING: ShardServerTiming = ShardServerTiming::default();
}

// Built once, not per shard op (#161): `StorageConfig::from_env()` does a
// real env lookup plus a match and a debug!/warn! log line every time, and
// while `LocalXFSBinaryStore::new()` itself is cheap (no fields), none of
// that work needs repeating on every single store_shard/load_shard call.
lazy_static! {
    static ref STORE: Arc<dyn Storage> = StorageConfig::from_env().create_store();
}

fn store() -> Arc<dyn Storage> {
    Arc::clone(&STORE)
}

/// Length-prefixes `bucket` (`"{bucket.len()}:{bucket}:..."`) so its exact
/// boundary is always recoverable regardless of what bytes `bucket` or
/// `key` contain, instead of relying on a separator (`__`) that a real
/// bucket or key can also contain. `key` doesn't need its own length
/// prefix: `shard_idx` is always a plain decimal string with no `:` in
/// it, so whatever follows the LAST `:` in the whole string is always
/// `shard_idx`, and everything between bucket's closing `:` and that last
/// `:` is always `key` -- unambiguous either way.
///
/// Previously `format!("{bucket}__{key}__shard{shard_idx}")`: two
/// different `(bucket, key)` pairs could collide onto the same shard key
/// whenever one's `__` happened to land inside the other's bucket or key
/// (e.g. bucket `a__b` + key `c` and bucket `a` + key `b__c` both produced
/// `a__b__c__shard0`), silently overwriting one object's shard-location
/// pointer with another's (#150).
/// A fresh, per-PUT identifier with no `:` in it (hex digits and `-`
/// only), so `versioned_key` can safely use `:` as a separator around it
/// without ambiguity. Not globally unique in the cryptographic sense —
/// a monotonic per-process counter paired with a nanosecond timestamp is
/// enough to guarantee two overwrites of the same key from the same
/// coordinator process never reuse a version id, which is all this
/// needs (#151).
pub fn new_version_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{nanos:x}-{seq:x}")
}

/// Folds a version id into a key string before it ever reaches
/// `shard_key`/`store_shard`/`load_shard` — so every version of an
/// object owns a completely distinct set of shard keys, never
/// overwriting another version's bytes in place (#151). Injective over
/// `(key, version)`: `version` is guaranteed `:`-free (see
/// `new_version_id`), so it's always exactly the maximal `:`-free suffix
/// of the result, regardless of what `key` itself contains.
pub fn versioned_key(key: &str, version: &str) -> String {
    format!("{key}:{version}")
}

pub fn shard_key(bucket: &str, key: &str, shard_idx: usize) -> String {
    format!("{}:{}:{}:{}", bucket.len(), bucket, key, shard_idx)
}

pub fn store_shard(bucket: &str, key: &str, shard_idx: usize, data: &[u8]) -> Result<(), Error> {
    let request_start = Instant::now();
    let sk = shard_key(bucket, key, shard_idx);

    let t0 = Instant::now();
    let (offset, size) = store().write(CLUSTER_SHARD_USER, bucket, data)?;
    SERVER_TIMING.storage_write.record(t0.elapsed());

    let t0 = Instant::now();
    SHARD_META_STORE
        .put(&sk, offset, size)
        .map_err(|e| ErrorInternalServerError(e.to_string()))?;
    SERVER_TIMING.meta_put.record(t0.elapsed());

    SERVER_TIMING.total.record(request_start.elapsed());
    Ok(())
}

pub fn load_shard(bucket: &str, key: &str, shard_idx: usize) -> Result<Vec<u8>, Error> {
    let sk = shard_key(bucket, key, shard_idx);
    let (offset, size) = SHARD_META_STORE
        .get(&sk)
        .ok_or_else(|| ErrorNotFound("shard key does not exist"))?;
    store().read(CLUSTER_SHARD_USER, bucket, offset, size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_then_load_round_trips() {
        let data = b"gRPC-path shard bytes, no SQLite mutex on this path anymore".to_vec();
        store_shard("shardtest-bucket", "shardtest-key", 0, &data).unwrap();
        let loaded = load_shard("shardtest-bucket", "shardtest-key", 0).unwrap();
        assert_eq!(loaded, data);
    }

    #[test]
    fn missing_shard_returns_not_found() {
        let err = load_shard("shardtest-bucket", "never-written-key", 0).unwrap_err();
        assert_eq!(err.as_response_error().status_code(), actix_web::http::StatusCode::NOT_FOUND);
    }

    /// #151: two overwrites of the same key must get distinct versioned
    /// keys, so their shard writes never collide.
    #[test]
    fn new_version_id_is_distinct_across_calls() {
        let a = new_version_id();
        let b = new_version_id();
        assert_ne!(a, b);
        assert!(!a.contains(':'));
        assert!(!b.contains(':'));
    }

    #[test]
    fn versioned_key_is_injective_over_key_and_version() {
        assert_ne!(versioned_key("foo", "v1"), versioned_key("foo", "v2"));
        assert_ne!(versioned_key("foo", "v1"), versioned_key("foo:v1", "x"));
        assert_ne!(versioned_key("a", "bc"), versioned_key("a:b", "c"));
    }

    /// #151's actual fix, end to end at this module's level: a shard
    /// written under one version is unreachable through a different
    /// version's wire key, even for the exact same (bucket, key, idx) --
    /// this is what makes an overwrite's shard writes incapable of
    /// corrupting a still-pinned previous version's bytes.
    #[test]
    fn overwriting_under_a_new_version_does_not_disturb_the_old_version() {
        let old_data = b"version one bytes".to_vec();
        let new_data = b"version two bytes, different length".to_vec();
        let old_key = versioned_key("versiontest-key", "v1");
        let new_key = versioned_key("versiontest-key", "v2");

        store_shard("versiontest-bucket", &old_key, 0, &old_data).unwrap();
        store_shard("versiontest-bucket", &new_key, 0, &new_data).unwrap();

        assert_eq!(load_shard("versiontest-bucket", &old_key, 0).unwrap(), old_data);
        assert_eq!(load_shard("versiontest-bucket", &new_key, 0).unwrap(), new_data);
    }

    /// #150: the exact collision from the issue's repro -- a bucket/key
    /// split that used to land on the same shard key once `__` was used
    /// as the separator.
    #[test]
    fn shard_key_does_not_collide_when_a_separator_moves_across_the_bucket_key_boundary() {
        assert_ne!(shard_key("a__b", "c", 0), shard_key("a", "b__c", 0));
    }

    #[test]
    fn shard_key_is_injective_over_bucket_key_and_shard_idx() {
        let cases: &[(&str, &str, usize)] = &[
            ("a", "b", 0),
            ("a", "b", 1),
            ("a", "bb", 0),
            ("aa", "b", 0),
            ("a:1", "b", 1),
            ("a", "1:b", 1),
            ("a:b", "1", 1),
        ];
        for (i, &(b1, k1, i1)) in cases.iter().enumerate() {
            for &(b2, k2, i2) in &cases[i + 1..] {
                assert_ne!(
                    shard_key(b1, k1, i1),
                    shard_key(b2, k2, i2),
                    "collision between ({b1:?},{k1:?},{i1}) and ({b2:?},{k2:?},{i2})"
                );
            }
        }
    }
}
