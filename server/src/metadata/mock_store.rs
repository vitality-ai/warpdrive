//! Mock implementation of MetadataStorage trait for testing

use crate::metadata::{MetadataStorage, Metadata, ObjectId};
use actix_web::Error;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Mock implementation of MetadataStorage for testing
pub struct MockMetadataStore {
    data: Arc<Mutex<HashMap<String, HashMap<String, HashMap<String, Metadata>>>>>,
}

impl MockMetadataStore {
    /// Create a new mock metadata store
    pub fn new() -> Self {
        Self {
            data: Arc::new(Mutex::new(HashMap::new())),
        }
    }
    
    /// Clear all data from the store (useful for test cleanup)
    pub fn clear(&self) {
        let mut data = self.data.lock().unwrap();
        data.clear();
    }
    
    /// Get the number of users in the store
    pub fn user_count(&self) -> usize {
        let data = self.data.lock().unwrap();
        data.len()
    }
    
    /// Get the number of objects for a specific user
    pub fn object_count(&self, user_id: &str) -> usize {
        let data = self.data.lock().unwrap();
        if let Some(user_data) = data.get(user_id) {
            user_data.values().map(|bucket_data| bucket_data.len()).sum()
        } else {
            0
        }
    }
}

impl Default for MockMetadataStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MetadataStorage for MockMetadataStore {
    fn put_metadata(&self, user_id: &str, bucket: &str, object_id: &str, metadata: &Metadata) -> Result<(), Error> {
        let mut data = self.data.lock().unwrap();
        let user_data = data.entry(user_id.to_string()).or_insert_with(HashMap::new);
        let bucket_data = user_data.entry(bucket.to_string()).or_insert_with(HashMap::new);
        
        if bucket_data.contains_key(object_id) {
            return Err(actix_web::error::ErrorBadRequest("Key already exists"));
        }
        
        bucket_data.insert(object_id.to_string(), metadata.clone());
        Ok(())
    }
    
    fn get_metadata(&self, user_id: &str, bucket: &str, object_id: &str) -> Result<Metadata, Error> {
        let data = self.data.lock().unwrap();
        
        if let Some(user_data) = data.get(user_id) {
            if let Some(bucket_data) = user_data.get(bucket) {
                if let Some(metadata) = bucket_data.get(object_id) {
                    return Ok(metadata.clone());
                }
            }
        }
        
        Err(actix_web::error::ErrorNotFound(format!(
            "No data found for key: {} in bucket: {}, The key does not exist", 
            object_id, bucket
        )))
    }
    
    fn delete_metadata(&self, user_id: &str, bucket: &str, object_id: &str) -> Result<(), Error> {
        let mut data = self.data.lock().unwrap();
        
        if let Some(user_data) = data.get_mut(user_id) {
            if let Some(bucket_data) = user_data.get_mut(bucket) {
                if bucket_data.remove(object_id).is_some() {
                    return Ok(());
                }
            }
        }
        
        Err(actix_web::error::ErrorNotFound(format!(
            "No data found for key: {} in bucket: {}, The key does not exist", 
            object_id, bucket
        )))
    }
    
    fn list_objects(&self, user_id: &str, bucket: &str) -> Result<Vec<ObjectId>, Error> {
        let data = self.data.lock().unwrap();
        
        if let Some(user_data) = data.get(user_id) {
            if let Some(bucket_data) = user_data.get(bucket) {
                Ok(bucket_data.keys().cloned().collect())
            } else {
                Ok(Vec::new())
            }
        } else {
            Ok(Vec::new())
        }
    }
    
    fn object_exists(&self, user_id: &str, bucket: &str, object_id: &str) -> Result<bool, Error> {
        let data = self.data.lock().unwrap();
        
        if let Some(user_data) = data.get(user_id) {
            if let Some(bucket_data) = user_data.get(bucket) {
                Ok(bucket_data.contains_key(object_id))
            } else {
                Ok(false)
            }
        } else {
            Ok(false)
        }
    }
    
    fn update_metadata(&self, user_id: &str, bucket: &str, object_id: &str, metadata: &Metadata) -> Result<(), Error> {
        let mut data = self.data.lock().unwrap();
        
        if let Some(user_data) = data.get_mut(user_id) {
            if let Some(bucket_data) = user_data.get_mut(bucket) {
                if bucket_data.contains_key(object_id) {
                    bucket_data.insert(object_id.to_string(), metadata.clone());
                    return Ok(());
                }
            }
        }
        
        Err(actix_web::error::ErrorNotFound(format!(
            "No data found for key: {}, The key does not exist", 
            object_id
        )))
    }
    
    fn update_object_id(&self, user_id: &str, bucket: &str, old_object_id: &str, new_object_id: &str) -> Result<(), Error> {
        let mut data = self.data.lock().unwrap();
        
        if let Some(user_data) = data.get_mut(user_id) {
            if let Some(bucket_data) = user_data.get_mut(bucket) {
                if let Some(metadata) = bucket_data.remove(old_object_id) {
                    bucket_data.insert(new_object_id.to_string(), metadata);
                    return Ok(());
                }
            }
        }
        
        Err(actix_web::error::ErrorNotFound(format!(
            "No data found for key: {}, The key does not exist", 
            old_object_id
        )))
    }
    
    fn queue_deletion(&self, user_id: &str, bucket: &str, key: &str, offset_size_list: &[(u64, u64)]) -> Result<(), Error> {
        // For mock store, just return Ok (no-op for testing)
        // In a real implementation, this would queue the deletion for background processing
        Ok(())
    }
    
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mock_metadata_store_basic_operations() {
        let store = MockMetadataStore::new();
        
        // Create test metadata
        let mut metadata = Metadata::from_offset_size_list(vec![(100, 200), (300, 400)]);
        metadata.properties.insert("test_prop".to_string(), "test_value".to_string());
        
        let user_id = "test_user_mock";
        let object_id = "test_object_mock";
        
        // Initially should be empty
        assert_eq!(store.user_count(), 0);
        assert_eq!(store.object_count(user_id), 0);
        
        // Test put_metadata
        store.put_metadata(user_id, "default", object_id, &metadata).unwrap();
        assert_eq!(store.user_count(), 1);
        assert_eq!(store.object_count(user_id), 1);
        
        // Test put_metadata with duplicate key should fail
        let result = store.put_metadata(user_id, "default", object_id, &metadata);
        assert!(result.is_err());
        
        // Test object_exists
        assert!(store.object_exists(user_id, "default", object_id).unwrap());
        assert!(!store.object_exists(user_id, "default", "nonexistent").unwrap());
        
        // Test get_metadata
        let retrieved = store.get_metadata(user_id, "default", object_id).unwrap();
        assert_eq!(retrieved.chunks.len(), 2);
        assert_eq!(retrieved.to_offset_size_list(), vec![(100, 200), (300, 400)]);
        assert_eq!(retrieved.properties.get("test_prop"), Some(&"test_value".to_string()));
        
        // Test list_objects
        let objects = store.list_objects(user_id, "default").unwrap();
        assert_eq!(objects.len(), 1);
        assert!(objects.contains(&object_id.to_string()));
        
        // Test update_metadata
        let new_metadata = Metadata::from_offset_size_list(vec![(500, 600)]);
        store.update_metadata(user_id, "default", object_id, &new_metadata).unwrap();
        
        let updated = store.get_metadata(user_id, "default", object_id).unwrap();
        assert_eq!(updated.to_offset_size_list(), vec![(500, 600)]);
        
        // Test update_object_id
        let new_object_id = "new_test_object_mock";
        store.update_object_id(user_id, "default", object_id, new_object_id).unwrap();
        
        assert!(!store.object_exists(user_id, "default", object_id).unwrap());
        assert!(store.object_exists(user_id, "default", new_object_id).unwrap());
        
        // Test delete_metadata
        store.delete_metadata(user_id, "default", new_object_id).unwrap();
        assert!(!store.object_exists(user_id, "default", new_object_id).unwrap());
        assert_eq!(store.object_count(user_id), 0);
        
        // Test clear
        store.put_metadata(user_id, "default", object_id, &metadata).unwrap();
        assert_eq!(store.object_count(user_id), 1);
        store.clear();
        assert_eq!(store.user_count(), 0);
        assert_eq!(store.object_count(user_id), 0);
    }
    
    #[test]
    fn test_mock_metadata_store_error_cases() {
        let store = MockMetadataStore::new();
        let user_id = "test_user";
        let object_id = "test_object";
        
        // Test get_metadata for nonexistent object
        let result = store.get_metadata(user_id, "default", object_id);
        assert!(result.is_err());
        
        // Test delete_metadata for nonexistent object
        let result = store.delete_metadata(user_id, "default", object_id);
        assert!(result.is_err());
        
        // Test update_metadata for nonexistent object
        let metadata = Metadata::from_offset_size_list(vec![(100, 200)]);
        let result = store.update_metadata(user_id, "default", object_id, &metadata);
        assert!(result.is_err());
        
        // Test update_object_id for nonexistent object
        let result = store.update_object_id(user_id, "default", object_id, "new_id");
        assert!(result.is_err());
        
        // Test list_objects for nonexistent user
        let objects = store.list_objects("nonexistent_user", "default").unwrap();
        assert!(objects.is_empty());
    }
}