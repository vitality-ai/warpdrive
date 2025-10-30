//! Binary Storage Layer Abstraction
//!
//! This module defines the storage trait that concrete backends implement.

pub mod local_store;
pub mod mock_store;
pub mod config;
pub mod deletion_worker;

use actix_web::Error;

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