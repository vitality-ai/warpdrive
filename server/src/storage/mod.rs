//! Binary Storage Layer Abstraction
//!
//! This module defines the storage trait that concrete backends implement.

pub mod local_store;
pub mod mock_store;
pub mod config;
pub mod group_commit;
pub mod kv_engine;
pub mod durable_log;

use actix_web::Error;
use std::sync::{Arc, OnceLock};

/// Trait defining the minimal binary storage interface
pub trait Storage: Send + Sync {
    /// Write `data` for a `user_id` and `bucket`, returning (offset, size)
    fn write(&self, user_id: &str, bucket: &str, data: &[u8]) -> Result<(u64, u64), Error>;

    /// Read `size` bytes from `offset` for a `user_id` and `bucket`
    fn read(&self, user_id: &str, bucket: &str, offset: u64, size: u64) -> Result<Vec<u8>, Error>;

    /// Delete previously written ranges by queuing/logging deletion for background processing
    fn delete(&self, user_id: &str, bucket: &str, offset_size_list: &[(u64, u64)]) -> Result<(), Error>;

    /// Verify data integrity for the specified range
    fn verify(&self, user_id: &str, bucket: &str, offset: u64, size: u64, checksum: &[u8]) -> Result<bool, Error>;

    /// `read` as a future the caller awaits on its own thread, for stores
    /// whose read waits on the network. `None`: use `read`, which blocks.
    /// `write` as a future, for stores whose write waits on the network:
    /// a PUT awaits it on its HTTP worker instead of parking a thread.
    /// `None` means the store has no async path and the caller uses `write`.
    fn write_async(&self, _user_id: &str, _bucket: &str, _data: Vec<u8>) -> Option<futures::future::LocalBoxFuture<'static, Result<(u64, u64), String>>> {
        None
    }

    fn read_async(&self, _user_id: &str, _bucket: &str, _offset: u64, _size: u64) -> Option<futures::future::LocalBoxFuture<'static, Result<Vec<u8>, String>>> {
        None
    }
}
static OBJECT_STORE: OnceLock<Arc<dyn Storage>> = OnceLock::new();

/// Sets the store the S3 handlers keep object bytes in. Called once at
/// startup: a cluster node sets the cluster store here, so the same
/// handlers serve both deployments. Returns false if already set.
pub fn set_object_store(store: Arc<dyn Storage>) -> bool {
    OBJECT_STORE.set(store).is_ok()
}

/// The store the S3 handlers keep object bytes in. Without
/// `set_object_store`, the one `STORAGE_BACKEND` names (local disk by
/// default). The cluster's own shard bytes don't go through this: each node
/// stores its shards on its local disk (`cluster::shard_storage`).
pub fn object_store() -> Arc<dyn Storage> {
    Arc::clone(OBJECT_STORE.get_or_init(|| config::StorageConfig::from_env().create_store()))
}
