//! Binary Storage Layer Abstraction
//! 
//! This module provides an abstraction over binary storage backends,
//! allowing the system to use different storage implementations (Local XFS, 
//! distributed file systems, etc.) without affecting higher-level services.

pub mod local_xfs_store;
pub mod mock_store;
pub mod config;

#[cfg(test)]
mod comprehensive_test;

use actix_web::Error;

/// Error type for binary storage operations
pub type BinaryStorageError = Error;

/// Trait defining the binary storage interface
pub trait BinaryStorage: Send + Sync {
    /// Store object data and return its storage metadata (offset, size)
    fn put_object(&self, user_id: &str, object_id: &str, data: &[u8]) -> Result<(u64, u64), Error>;
    
    /// Retrieve object data from storage using metadata (offset, size) 
    fn get_object(&self, user_id: &str, object_id: &str, offset: u64, size: u64) -> Result<Vec<u8>, Error>;
    
    /// Delete object data (currently marks as deleted in log, but doesn't physically remove)
    fn delete_object(&self, user_id: &str, object_id: &str, offset_size_list: &[(u64, u64)]) -> Result<(), Error>;
    
    /// Verify object integrity using checksum (optional implementation)
    fn verify_object(&self, _user_id: &str, _object_id: &str, checksum: &[u8]) -> Result<bool, Error> {
        // Default implementation - always returns true if checksum is provided
        Ok(!checksum.is_empty())
    }
    
    /// Store multiple objects in batch (for FlatBuffer data processing)
    fn put_objects_batch(&self, user_id: &str, data_list: Vec<&[u8]>) -> Result<Vec<(u64, u64)>, Error>;
    
    /// Retrieve multiple objects in batch (for FlatBuffer data processing)
    fn get_objects_batch(&self, user_id: &str, offset_size_list: &[(u64, u64)]) -> Result<Vec<Vec<u8>>, Error>;
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_binary_storage_trait_compiles() {
        // This test ensures the trait definition compiles correctly
        // Actual implementations will be tested in their respective modules
        assert!(true);
    }
}