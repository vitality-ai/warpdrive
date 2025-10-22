//! Metadata service layer that bridges the old Database interface with the new MetadataStorage trait

use crate::metadata::{MetadataStorage, Metadata, config::MetadataConfig};
use std::sync::Arc;
use actix_web::Error;
use lazy_static::lazy_static;

lazy_static! {
    static ref METADATA_STORE: Arc<dyn MetadataStorage> = {
        let config = MetadataConfig::from_env();
        config.create_store()
    };
}

/// Metadata service that provides database-like interface using the abstracted storage
pub struct MetadataService {
    user: String,
}

impl MetadataService {
    /// Create a new metadata service for a specific user
    pub fn new(user: &str) -> Result<Self, Error> {
        Ok(Self {
            user: user.to_string(),
        })
    }
    
    /// Check if a key exists
    pub fn check_key(&self, bucket: &str, key: &str) -> Result<bool, Error> {
        METADATA_STORE.object_exists(&self.user, bucket, key)
    }
    
    /// Check key non-existence and return error if it doesn't exist
    pub fn check_key_nonexistance(&self, bucket: &str, key: &str) -> Result<(), Error> {
        if !self.check_key(bucket, key)? {
            return Err(actix_web::error::ErrorNotFound(format!(
                "No data found for key: {} in bucket: {}, The key does not exist", 
                key, bucket
            )));
        }
        Ok(())
    }
    
    /// Write metadata for a key with offset-size list
    pub fn write_metadata(&self, bucket: &str, key: &str, offset_size_bytes: &[u8]) -> Result<(), Error> {
        use crate::util::serializer::deserialize_offset_size;
        let offset_size_list = deserialize_offset_size(offset_size_bytes)?;
        let metadata = Metadata::from_offset_size_list(offset_size_list);
        METADATA_STORE.put_metadata(&self.user, bucket, key, &metadata)
    }
    
    /// Read metadata for a key
    pub fn read_metadata(&self, bucket: &str, key: &str) -> Result<Vec<u8>, Error> {
        use crate::util::serializer::serialize_offset_size;
        let metadata = METADATA_STORE.get_metadata(&self.user, bucket, key)?;
        let offset_size_list = metadata.to_offset_size_list();
        serialize_offset_size(&offset_size_list)
    }
    
    /// Delete metadata for a key
    pub fn delete_metadata(&self, bucket: &str, key: &str) -> Result<(), Error> {
        METADATA_STORE.delete_metadata(&self.user, bucket, key)
    }
    
    /// Rename a key
    pub fn rename_key(&self, bucket: &str, old_key: &str, new_key: &str) -> Result<(), Error> {
        METADATA_STORE.update_object_id(&self.user, bucket, old_key, new_key)
    }
    
    /// Update metadata for an existing key
    pub fn update_metadata(&self, bucket: &str, key: &str, offset_size_bytes: &[u8]) -> Result<(), Error> {
        use crate::util::serializer::deserialize_offset_size;
        let offset_size_list = deserialize_offset_size(offset_size_bytes)?;
        let metadata = Metadata::from_offset_size_list(offset_size_list);
        METADATA_STORE.update_metadata(&self.user, bucket, key, &metadata)
    }
    
    /// Append data (same as update for now)
    pub fn append_metadata(&self, bucket: &str, key: &str, offset_size_bytes: &[u8]) -> Result<(), Error> {
        self.update_metadata(bucket, key, offset_size_bytes)
    }
    
    /// List objects in a bucket
    pub fn list_objects(&self, bucket: &str) -> Result<Vec<String>, Error> {
        METADATA_STORE.list_objects(&self.user, bucket)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::serializer::serialize_offset_size;
    use std::env;

    #[test]
    fn test_metadata_service_basic_operations() {
        // Set up mock backend for testing
        env::set_var("METADATA_BACKEND", "mock");
        
        let service = MetadataService::new("test_user_service").unwrap();
        let key = "test_key_service";
        
        // Initially key should not exist
        assert!(!service.check_key("default", key).unwrap());
        assert!(service.check_key_nonexistance("default", key).is_err());
        
        // Create test data
        let offset_size_list = vec![(100, 200), (300, 400)];
        let offset_size_bytes = serialize_offset_size(&offset_size_list).unwrap();
        
        // Upload data
        service.write_metadata("default", key, &offset_size_bytes).unwrap();
        
        // Key should now exist
        assert!(service.check_key("default", key).unwrap());
        assert!(service.check_key_nonexistance("default", key).is_ok());
        
        // Retrieve data
        let retrieved_bytes = service.read_metadata("default", key).unwrap();
        assert_eq!(retrieved_bytes, offset_size_bytes);
        
        // Update data
        let new_offset_size_list = vec![(500, 600)];
        let new_offset_size_bytes = serialize_offset_size(&new_offset_size_list).unwrap();
        service.update_metadata("default", key, &new_offset_size_bytes).unwrap();
        
        let updated_bytes = service.read_metadata("default", key).unwrap();
        assert_eq!(updated_bytes, new_offset_size_bytes);
        
        // Update key name
        let new_key = "new_test_key_service";
        service.rename_key("default", key, new_key).unwrap();
        
        assert!(!service.check_key("default", key).unwrap());
        assert!(service.check_key("default", new_key).unwrap());
        
        // Delete data
        service.delete_metadata("default", new_key).unwrap();
        assert!(!service.check_key("default", new_key).unwrap());
        
        // Clean up
        env::remove_var("METADATA_BACKEND");
    }
}