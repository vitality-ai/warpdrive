//! Metadata Storage Layer Abstraction
//! 
//! This module provides an abstraction over metadata storage backends,
//! allowing the system to use different storage implementations (SQLite, 
//! distributed databases, etc.) without affecting higher-level services.

pub mod sqlite_store;
pub mod mock_store;
pub mod config;

#[cfg(test)]
mod comprehensive_test;

use actix_web::Error;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Represents the location and properties of stored data chunks
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DataChunk {
    /// Offset in the storage file where this chunk starts
    pub offset: u64,
    /// Size of the chunk in bytes
    pub size: u64,
}

/// Metadata associated with a stored object
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Metadata {
    /// List of data chunks that comprise this object
    pub chunks: Vec<DataChunk>,
    /// Optional additional metadata that can be extended for future use
    pub properties: HashMap<String, String>,
}

impl Metadata {
    /// Create a new metadata instance from offset-size list
    pub fn from_offset_size_list(offset_size_list: Vec<(u64, u64)>) -> Self {
        let chunks = offset_size_list
            .into_iter()
            .map(|(offset, size)| DataChunk { offset, size })
            .collect();
        
        Self {
            chunks,
            properties: HashMap::new(),
        }
    }
    
    /// Convert metadata to offset-size list for backward compatibility
    pub fn to_offset_size_list(&self) -> Vec<(u64, u64)> {
        self.chunks
            .iter()
            .map(|chunk| (chunk.offset, chunk.size))
            .collect()
    }
}

/// Object identifier type
pub type ObjectId = String;

/// User identifier type 
pub type UserId = String;

/// Trait defining the metadata storage interface
pub trait MetadataStorage: Send + Sync {
    /// Store metadata for an object
    fn put_metadata(&self, user_id: &str, object_id: &str, metadata: &Metadata) -> Result<(), Error>;
    
    /// Retrieve metadata for an object
    fn get_metadata(&self, user_id: &str, object_id: &str) -> Result<Metadata, Error>;
    
    /// Delete metadata for an object
    fn delete_metadata(&self, user_id: &str, object_id: &str) -> Result<(), Error>;
    
    /// List all objects for a user
    fn list_objects(&self, user_id: &str) -> Result<Vec<ObjectId>, Error>;
    
    /// Check if an object exists
    fn object_exists(&self, user_id: &str, object_id: &str) -> Result<bool, Error>;
    
    /// Update metadata for an existing object
    fn update_metadata(&self, user_id: &str, object_id: &str, metadata: &Metadata) -> Result<(), Error>;
    
    /// Update the key (object_id) for an existing object
    fn update_object_id(&self, user_id: &str, old_object_id: &str, new_object_id: &str) -> Result<(), Error>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metadata_from_offset_size_list() {
        let offset_size_list = vec![(100, 200), (300, 400)];
        let metadata = Metadata::from_offset_size_list(offset_size_list.clone());
        
        assert_eq!(metadata.chunks.len(), 2);
        assert_eq!(metadata.chunks[0].offset, 100);
        assert_eq!(metadata.chunks[0].size, 200);
        assert_eq!(metadata.chunks[1].offset, 300);
        assert_eq!(metadata.chunks[1].size, 400);
        
        let back_to_list = metadata.to_offset_size_list();
        assert_eq!(back_to_list, offset_size_list);
    }
    
    #[test]
    fn test_data_chunk_equality() {
        let chunk1 = DataChunk { offset: 100, size: 200 };
        let chunk2 = DataChunk { offset: 100, size: 200 };
        let chunk3 = DataChunk { offset: 100, size: 300 };
        
        assert_eq!(chunk1, chunk2);
        assert_ne!(chunk1, chunk3);
    }
}