//! Binary Storage Layer Abstraction
//! 
//! This module provides an abstraction over binary storage backends,
//! allowing the system to use different storage implementations (local files,
//! distributed file systems, etc.) without affecting higher-level services.

pub mod local_store;
pub mod mock_store;
pub mod deletion_worker;

#[cfg(test)]
mod comprehensive_test;

use actix_web::Error;

/// Trait defining the binary storage interface
pub trait Storage: Send + Sync {
    /// Write data for a user, bucket and return (offset, size) - simulates the original append behavior
    fn write_data(&self, user_id: &str, bucket: &str, data: &[u8]) -> Result<(u64, u64), Error>;
    
    /// Read data for a user, bucket from specific offset and size
    fn read_data(&self, user_id: &str, bucket: &str, offset: u64, size: u64) -> Result<Vec<u8>, Error>;
    
    /// Delete object data for a user, bucket with key and offset/size information
    fn delete_object(&self, user_id: &str, bucket: &str, key: &str, offset_size_list: &[(u64, u64)]) -> Result<(), Error>;
    
    /// Verify object integrity using checksum
    fn verify_object(&self, user_id: &str, bucket: &str, key: &str, checksum: &[u8]) -> Result<bool, Error>;
}

/// Object identifier type
pub type ObjectId = String;

/// User identifier type 
pub type UserId = String;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_storage_trait_interface() {
        // This test verifies that the Storage trait is properly defined
        // Implementation tests are in the specific storage backend modules
        assert!(true);
    }
}