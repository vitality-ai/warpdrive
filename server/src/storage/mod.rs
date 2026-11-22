//! Binary Storage Layer Abstraction
//!
//! This module defines the storage trait that concrete backends implement.

pub mod local_store;
pub mod mock_store;
pub mod config;

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

/// Reads through the store's async path when it has one (the cluster: the
/// read is awaited on this worker), else on the blocking pool.
pub async fn read_any(store: Arc<dyn Storage>, user_id: String, bucket: String, offset: u64, size: u64) -> Result<Vec<u8>, String> {
    if let Some(fut) = store.read_async(&user_id, &bucket, offset, size) {
        return fut.await;
    }
    actix_web::web::block(move || store.read(&user_id, &bucket, offset, size).map_err(|e| e.to_string()))
        .await
        .map_err(|e| format!("blocking read task failed: {e}"))?
}

/// Writes through the store's async path when it has one, else on the
/// blocking pool. Returns once the bytes are durable.
pub async fn write_any(store: Arc<dyn Storage>, user_id: String, bucket: String, data: Vec<u8>) -> Result<(u64, u64), String> {
    if let Some(fut) = store.write_async(&user_id, &bucket, data.clone()) {
        return fut.await;
    }
    actix_web::web::block(move || store.write(&user_id, &bucket, &data).map_err(|e| e.to_string()))
        .await
        .map_err(|e| format!("blocking write task failed: {e}"))?
}

/// An object's bytes from its extents, each read through `read_any`.
pub async fn read_extents_any(store: Arc<dyn Storage>, user_id: &str, bucket: &str, extents: &[(u64, u64)]) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(extents.iter().map(|(_, n)| *n as usize).sum());
    for &(offset, size) in extents {
        out.extend(read_any(Arc::clone(&store), user_id.to_string(), bucket.to_string(), offset, size).await?);
    }
    Ok(out)
}
