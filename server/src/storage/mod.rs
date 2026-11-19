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
