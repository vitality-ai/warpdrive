//! metadata.rs
//! 
//! Metadata Storage Abstraction Layer (META)
//! 
//! This module provides an abstraction layer for metadata storage that enables
//! swapping between different backend implementations (SQLite, distributed databases, etc.)
//! without affecting higher-level services.

use actix_web::Error;
use serde::{Deserialize, Serialize};

/// Represents metadata for a stored object, including its binary locations
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Metadata {
    /// List of (offset, size) pairs indicating where chunks of the object are stored
    pub offset_size_list: Vec<(u64, u64)>,
}

impl Metadata {
    /// Create new metadata from offset-size pairs
    #[allow(dead_code)] // Used in tests
    pub fn new(offset_size_list: Vec<(u64, u64)>) -> Self {
        Self { offset_size_list }
    }

    /// Get the serialized representation of the metadata
    pub fn to_bytes(&self) -> Result<Vec<u8>, Error> {
        crate::util::serializer::serialize_offset_size(&self.offset_size_list)
    }

    /// Create metadata from serialized bytes
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        let offset_size_list = crate::util::serializer::deserialize_offset_size(bytes)?;
        Ok(Self { offset_size_list })
    }
}

/// Object identifier type alias for clarity
pub type ObjectId = String;

/// User identifier type alias for clarity  
pub type UserId = String;

/// Abstract interface for metadata storage backends
pub trait MetadataStorage: Send + Sync {
    /// Store metadata for an object
    /// 
    /// # Arguments
    /// * `user_id` - The user who owns the object
    /// * `object_id` - Unique identifier for the object (key)
    /// * `metadata` - The metadata to store
    /// 
    /// # Returns
    /// * `Ok(())` if successful
    /// * `Err(Error)` if the operation fails
    fn put_metadata(&self, user_id: &str, object_id: &str, metadata: &Metadata) -> Result<(), Error>;

    /// Retrieve metadata for an object
    /// 
    /// # Arguments
    /// * `user_id` - The user who owns the object
    /// * `object_id` - Unique identifier for the object (key)
    /// 
    /// # Returns
    /// * `Ok(Metadata)` if the object exists
    /// * `Err(Error)` if the object doesn't exist or operation fails
    fn get_metadata(&self, user_id: &str, object_id: &str) -> Result<Metadata, Error>;

    /// Delete metadata for an object
    /// 
    /// # Arguments
    /// * `user_id` - The user who owns the object
    /// * `object_id` - Unique identifier for the object (key)
    /// 
    /// # Returns
    /// * `Ok(())` if successful
    /// * `Err(Error)` if the operation fails
    fn delete_metadata(&self, user_id: &str, object_id: &str) -> Result<(), Error>;

    /// Check if an object exists
    /// 
    /// # Arguments
    /// * `user_id` - The user who owns the object
    /// * `object_id` - Unique identifier for the object (key)
    /// 
    /// # Returns
    /// * `Ok(true)` if the object exists
    /// * `Ok(false)` if the object doesn't exist
    /// * `Err(Error)` if the operation fails
    fn exists(&self, user_id: &str, object_id: &str) -> Result<bool, Error>;

    /// Update the metadata for an existing object
    /// 
    /// # Arguments
    /// * `user_id` - The user who owns the object
    /// * `object_id` - Unique identifier for the object (key)
    /// * `metadata` - The new metadata to store
    /// 
    /// # Returns
    /// * `Ok(())` if successful
    /// * `Err(Error)` if the operation fails
    fn update_metadata(&self, user_id: &str, object_id: &str, metadata: &Metadata) -> Result<(), Error>;

    /// Update the object key/ID
    /// 
    /// # Arguments
    /// * `user_id` - The user who owns the object
    /// * `old_object_id` - Current identifier for the object
    /// * `new_object_id` - New identifier for the object
    /// 
    /// # Returns
    /// * `Ok(())` if successful
    /// * `Err(Error)` if the operation fails
    fn update_object_id(&self, user_id: &str, old_object_id: &str, new_object_id: &str) -> Result<(), Error>;

    /// List all objects for a user (optional - not needed for current functionality)
    /// 
    /// # Arguments
    /// * `user_id` - The user whose objects to list
    /// 
    /// # Returns
    /// * `Ok(Vec<ObjectId>)` list of object IDs
    /// * `Err(Error)` if the operation fails
    fn list_objects(&self, user_id: &str) -> Result<Vec<ObjectId>, Error>;
}