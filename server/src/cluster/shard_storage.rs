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

fn store() -> Arc<dyn Storage> {
    StorageConfig::from_env().create_store()
}

pub fn shard_key(bucket: &str, key: &str, shard_idx: usize) -> String {
    format!("{bucket}__{key}__shard{shard_idx}")
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
}
