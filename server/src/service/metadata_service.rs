//! Metadata service layer that bridges the old Database interface with the new MetadataStorage trait

use crate::metadata::{MetadataStorage, Metadata};
use crate::service::user_context::UserContext;
use std::sync::Arc;
use actix_web::Error;


/// Metadata service that provides database-like interface using the abstracted storage
pub struct MetadataService {
    metadata_store: Arc<dyn MetadataStorage>,
}

impl MetadataService {
    /// Create a new metadata service with injected metadata backend
    pub fn new(metadata_store: Arc<dyn MetadataStorage>) -> Self {
        Self {
            metadata_store,
        }
    }
    
    /// Check if a key exists
    pub fn check_key(&self, context: &UserContext, key: &str) -> Result<bool, Error> {
        self.metadata_store.object_exists(&context.user_id, &context.bucket, key)
    }
    
    /// Check key non-existence and return error if it doesn't exist
    pub fn check_key_nonexistance(&self, context: &UserContext, key: &str) -> Result<(), Error> {
        if !self.check_key(context, key)? {
            return Err(actix_web::error::ErrorNotFound(format!(
                "No data found for key: {} in bucket: {}, The key does not exist", 
                key, context.bucket
            )));
        }
        Ok(())
    }
    
    /// Write metadata for a key with offset-size list
    pub fn write_metadata(&self, context: &UserContext, key: &str, offset_size_bytes: &[u8]) -> Result<(), Error> {
        use crate::util::serializer::deserialize_offset_size;
        let offset_size_list = deserialize_offset_size(offset_size_bytes)?;
        let metadata = Metadata::from_offset_size_list(offset_size_list);
        self.metadata_store.put_metadata(&context.user_id, &context.bucket, key, &metadata)
    }
    
    /// Read metadata for a key
    pub fn read_metadata(&self, context: &UserContext, key: &str) -> Result<Vec<u8>, Error> {
        use crate::util::serializer::serialize_offset_size;
        let metadata = self.metadata_store.get_metadata(&context.user_id, &context.bucket, key)?;
        let offset_size_list = metadata.to_offset_size_list();
        serialize_offset_size(&offset_size_list)
    }
    
    /// Delete metadata for a key
    pub fn delete_metadata(&self, context: &UserContext, key: &str) -> Result<(), Error> {
        self.metadata_store.delete_metadata(&context.user_id, &context.bucket, key)
    }
    
    /// Rename a key
    pub fn rename_key(&self, context: &UserContext, old_key: &str, new_key: &str) -> Result<(), Error> {
        self.metadata_store.update_object_id(&context.user_id, &context.bucket, old_key, new_key)
    }
    
    /// Update metadata for an existing key
    pub fn update_metadata(&self, context: &UserContext, key: &str, offset_size_bytes: &[u8]) -> Result<(), Error> {
        use crate::util::serializer::deserialize_offset_size;
        let offset_size_list = deserialize_offset_size(offset_size_bytes)?;
        let metadata = Metadata::from_offset_size_list(offset_size_list);
        self.metadata_store.update_metadata(&context.user_id, &context.bucket, key, &metadata)
    }
    
    /// Append data (same as update for now)
    pub fn append_metadata(&self, context: &UserContext, key: &str, offset_size_bytes: &[u8]) -> Result<(), Error> {
        self.update_metadata(context, key, offset_size_bytes)
    }
    
    /// List objects in a bucket
    pub fn list_objects(&self, context: &UserContext) -> Result<Vec<String>, Error> {
        self.metadata_store.list_objects(&context.user_id, &context.bucket)
    }
    
    /// Queue deletion for background processing and delete metadata immediately
    pub fn queue_deletion(&self, user_id: &str, bucket: &str, key: &str, offset_size_list: &[(u64, u64)]) -> Result<(), Error> {
        // Queue the deletion for background processing
        self.metadata_store.queue_deletion(user_id, bucket, key, offset_size_list)?;
        
        // Delete metadata immediately
        self.metadata_store.delete_metadata(user_id, bucket, key)
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
        
        use crate::metadata::mock_store::MockMetadataStore;
        use std::sync::Arc;
        let metadata_store = Arc::new(MockMetadataStore::new());
        let service = MetadataService::new(metadata_store);
        let context = UserContext::with_bucket("test_user_service".to_string(), "default".to_string());
        let key = "test_key_service";
        
        // Initially key should not exist
        assert!(!service.check_key(&context, key).unwrap());
        assert!(service.check_key_nonexistance(&context, key).is_err());
        
        // Create test data
        let offset_size_list = vec![(100, 200), (300, 400)];
        let offset_size_bytes = serialize_offset_size(&offset_size_list).unwrap();
        
        // Upload data
        service.write_metadata(&context, key, &offset_size_bytes).unwrap();
        
        // Key should now exist
        assert!(service.check_key(&context, key).unwrap());
        assert!(service.check_key_nonexistance(&context, key).is_ok());
        
        // Retrieve data
        let retrieved_bytes = service.read_metadata(&context, key).unwrap();
        assert_eq!(retrieved_bytes, offset_size_bytes);
        
        // Update data
        let new_offset_size_list = vec![(500, 600)];
        let new_offset_size_bytes = serialize_offset_size(&new_offset_size_list).unwrap();
        service.update_metadata(&context, key, &new_offset_size_bytes).unwrap();
        
        let updated_bytes = service.read_metadata(&context, key).unwrap();
        assert_eq!(updated_bytes, new_offset_size_bytes);
        
        // Update key name
        let new_key = "new_test_key_service";
        service.rename_key(&context, key, new_key).unwrap();
        
        assert!(!service.check_key(&context, key).unwrap());
        assert!(service.check_key(&context, new_key).unwrap());
        
        // Delete data
        service.delete_metadata(&context, new_key).unwrap();
        assert!(!service.check_key(&context, new_key).unwrap());
        
        // Clean up
        env::remove_var("METADATA_BACKEND");
    }
}