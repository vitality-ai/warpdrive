//! Binary Storage Layer Abstraction
//!
//! This module defines the storage trait that concrete backends implement.

pub mod local_store;
pub mod mock_store;
pub mod slab_store;
pub mod config;

use actix_web::Error;

/// Trait defining the minimal binary storage interface
pub trait Storage: Send + Sync {
    /// Write `data` for a `user_id` and `bucket`, returning (offset, size).
    /// `slab_hint` is an opaque caller-supplied string (e.g. from the `x-warpd-slab` header)
    /// that slab-aware backends use to co-locate related objects; flat backends ignore it.
    fn write(&self, user_id: &str, bucket: &str, data: &[u8], slab_hint: Option<&str>) -> Result<(u64, u64), Error>;

    /// Read `size` bytes from `offset` for a `user_id` and `bucket`
    fn read(&self, user_id: &str, bucket: &str, offset: u64, size: u64) -> Result<Vec<u8>, Error>;

    /// Delete previously written ranges by queuing/logging deletion for background processing
    fn delete(&self, user_id: &str, bucket: &str, offset_size_list: &[(u64, u64)]) -> Result<(), Error>;

    /// Verify data integrity for the specified range
    fn verify(&self, user_id: &str, bucket: &str, offset: u64, size: u64, checksum: &[u8]) -> Result<bool, Error>;
}