//! mock_store.rs
//! 
//! Mock implementation of MetadataStorage for testing purposes.
//! This provides an in-memory implementation that doesn't require a database.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use actix_web::Error;

use crate::metadata::{MetadataStorage, Metadata, ObjectId};

/// Mock metadata storage using in-memory HashMap
#[derive(Clone)]
pub struct MockMetadataStore {
    /// Storage: (user_id, object_id) -> Metadata
    storage: Arc<Mutex<HashMap<(String, String), Metadata>>>,
}

impl MockMetadataStore {
    /// Create a new mock metadata store
    pub fn new() -> Self {
        Self {
            storage: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Clear all stored metadata (useful for testing)
    pub fn clear(&self) {
        let mut storage = self.storage.lock().unwrap();
        storage.clear();
    }

    /// Get the number of stored objects (useful for testing)
    pub fn len(&self) -> usize {
        let storage = self.storage.lock().unwrap();
        storage.len()
    }

    /// Check if the store is empty (useful for testing)
    pub fn is_empty(&self) -> bool {
        let storage = self.storage.lock().unwrap();
        storage.is_empty()
    }
}

impl Default for MockMetadataStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MetadataStorage for MockMetadataStore {
    fn put_metadata(&self, user_id: &str, object_id: &str, metadata: &Metadata) -> Result<(), Error> {
        let mut storage = self.storage.lock().unwrap();
        let key = (user_id.to_string(), object_id.to_string());
        
        // Check if object already exists
        if storage.contains_key(&key) {
            return Err(actix_web::error::ErrorBadRequest("Key already exists"));
        }
        
        storage.insert(key, metadata.clone());
        Ok(())
    }

    fn get_metadata(&self, user_id: &str, object_id: &str) -> Result<Metadata, Error> {
        let storage = self.storage.lock().unwrap();
        let key = (user_id.to_string(), object_id.to_string());
        
        storage.get(&key)
            .cloned()
            .ok_or_else(|| actix_web::error::ErrorNotFound(format!("No data found for key: {}, The key does not exist", object_id)))
    }

    fn delete_metadata(&self, user_id: &str, object_id: &str) -> Result<(), Error> {
        let mut storage = self.storage.lock().unwrap();
        let key = (user_id.to_string(), object_id.to_string());
        
        storage.remove(&key)
            .ok_or_else(|| actix_web::error::ErrorNotFound(format!("No data found for key: {}, The key does not exist", object_id)))?;
        
        Ok(())
    }

    fn exists(&self, user_id: &str, object_id: &str) -> Result<bool, Error> {
        let storage = self.storage.lock().unwrap();
        let key = (user_id.to_string(), object_id.to_string());
        Ok(storage.contains_key(&key))
    }

    fn update_metadata(&self, user_id: &str, object_id: &str, metadata: &Metadata) -> Result<(), Error> {
        let mut storage = self.storage.lock().unwrap();
        let key = (user_id.to_string(), object_id.to_string());
        
        if !storage.contains_key(&key) {
            return Err(actix_web::error::ErrorNotFound(format!("No data found for key: {}, The key does not exist", object_id)));
        }
        
        storage.insert(key, metadata.clone());
        Ok(())
    }

    fn update_object_id(&self, user_id: &str, old_object_id: &str, new_object_id: &str) -> Result<(), Error> {
        let mut storage = self.storage.lock().unwrap();
        let old_key = (user_id.to_string(), old_object_id.to_string());
        let new_key = (user_id.to_string(), new_object_id.to_string());
        
        // Check if old key exists
        let metadata = storage.remove(&old_key)
            .ok_or_else(|| actix_web::error::ErrorNotFound(format!("No data found for key: {}, The key does not exist", old_object_id)))?;
        
        // Check if new key already exists
        if storage.contains_key(&new_key) {
            // Restore the old key since we can't complete the operation
            storage.insert(old_key, metadata);
            return Err(actix_web::error::ErrorBadRequest("New key already exists"));
        }
        
        // Insert with new key
        storage.insert(new_key, metadata);
        Ok(())
    }

    fn list_objects(&self, user_id: &str) -> Result<Vec<ObjectId>, Error> {
        let storage = self.storage.lock().unwrap();
        let objects: Vec<ObjectId> = storage
            .keys()
            .filter(|(uid, _)| uid == user_id)
            .map(|(_, oid)| oid.clone())
            .collect();
        
        Ok(objects)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mock_store_basic_operations() {
        let store = MockMetadataStore::new();
        let metadata = Metadata::new(vec![(0, 100), (100, 200)]);
        
        // Test put_metadata
        assert!(store.put_metadata("user1", "key1", &metadata).is_ok());
        assert_eq!(store.len(), 1);
        
        // Test exists
        assert!(store.exists("user1", "key1").unwrap());
        assert!(!store.exists("user1", "key2").unwrap());
        
        // Test get_metadata
        let retrieved = store.get_metadata("user1", "key1").unwrap();
        assert_eq!(retrieved.offset_size_list, metadata.offset_size_list);
        
        // Test duplicate key
        assert!(store.put_metadata("user1", "key1", &metadata).is_err());
        
        // Test update_metadata
        let new_metadata = Metadata::new(vec![(0, 300)]);
        assert!(store.update_metadata("user1", "key1", &new_metadata).is_ok());
        let updated = store.get_metadata("user1", "key1").unwrap();
        assert_eq!(updated.offset_size_list, new_metadata.offset_size_list);
        
        // Test delete_metadata
        assert!(store.delete_metadata("user1", "key1").is_ok());
        assert!(!store.exists("user1", "key1").unwrap());
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn test_mock_store_update_object_id() {
        let store = MockMetadataStore::new();
        let metadata = Metadata::new(vec![(0, 100)]);
        
        store.put_metadata("user1", "old_key", &metadata).unwrap();
        
        // Test update_object_id
        assert!(store.update_object_id("user1", "old_key", "new_key").is_ok());
        assert!(!store.exists("user1", "old_key").unwrap());
        assert!(store.exists("user1", "new_key").unwrap());
        
        // Test update to existing key should fail
        store.put_metadata("user1", "another_key", &metadata).unwrap();
        assert!(store.update_object_id("user1", "new_key", "another_key").is_err());
    }

    #[test]
    fn test_mock_store_list_objects() {
        let store = MockMetadataStore::new();
        let metadata = Metadata::new(vec![(0, 100)]);
        
        store.put_metadata("user1", "key1", &metadata).unwrap();
        store.put_metadata("user1", "key2", &metadata).unwrap();
        store.put_metadata("user2", "key1", &metadata).unwrap();
        
        let user1_objects = store.list_objects("user1").unwrap();
        assert_eq!(user1_objects.len(), 2);
        assert!(user1_objects.contains(&"key1".to_string()));
        assert!(user1_objects.contains(&"key2".to_string()));
        
        let user2_objects = store.list_objects("user2").unwrap();
        assert_eq!(user2_objects.len(), 1);
        assert!(user2_objects.contains(&"key1".to_string()));
    }
}