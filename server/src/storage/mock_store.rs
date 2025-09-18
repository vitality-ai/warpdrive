//! Mock implementation of Storage trait for testing

use crate::storage::{Storage, ObjectId};
use actix_web::Error;
use actix_web::error::ErrorNotFound;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use log::info;

/// Mock implementation of Storage for testing
pub struct MockBinaryStore {
    // In-memory storage: user_id -> object_id -> data
    data: Arc<Mutex<HashMap<String, HashMap<String, Vec<u8>>>>>,
}

impl MockBinaryStore {
    pub fn new() -> Self {
        Self {
            data: Arc::new(Mutex::new(HashMap::new())),
        }
    }
    
    /// Get the number of users in the store
    pub fn user_count(&self) -> usize {
        let data = self.data.lock().unwrap();
        data.len()
    }
    
    /// Get the number of objects for a specific user
    pub fn object_count(&self, user_id: &str) -> usize {
        let data = self.data.lock().unwrap();
        data.get(user_id).map(|objects| objects.len()).unwrap_or(0)
    }
    
    /// Clear all data from the store
    pub fn clear(&self) {
        let mut data = self.data.lock().unwrap();
        data.clear();
    }
    
    /// Check if a user exists in the store
    pub fn user_exists(&self, user_id: &str) -> bool {
        let data = self.data.lock().unwrap();
        data.contains_key(user_id)
    }
    
    /// List all objects for a user
    pub fn list_objects(&self, user_id: &str) -> Vec<ObjectId> {
        let data = self.data.lock().unwrap();
        data.get(user_id)
            .map(|objects| objects.keys().cloned().collect())
            .unwrap_or_default()
    }
}

impl Default for MockBinaryStore {
    fn default() -> Self {
        Self::new()
    }
}

impl Storage for MockBinaryStore {
    fn write_data(&self, user_id: &str, _bucket: &str, data: &[u8]) -> Result<(u64, u64), Error> {
        // Simulate append behavior with virtual offset/size
        let mut store = self.data.lock().unwrap();
        let user_objects = store.entry(user_id.to_string()).or_insert_with(HashMap::new);
        
        // Calculate a virtual offset based on existing data size
        let offset = user_objects.values().map(|data| data.len() as u64).sum::<u64>();
        let size = data.len() as u64;
        
        // Store with a unique key based on offset
        let key = format!("chunk_{}", offset);
        user_objects.insert(key, data.to_vec());
        
        info!("Mock: Wrote data for user {} at virtual offset {} with size {}", 
              user_id, offset, size);
        
        Ok((offset, size))
    }
    
    fn read_data(&self, user_id: &str, _bucket: &str, offset: u64, size: u64) -> Result<Vec<u8>, Error> {
        // For mock, try to find data by virtual offset
        let store = self.data.lock().unwrap();
        
        if let Some(user_objects) = store.get(user_id) {
            let key = format!("chunk_{}", offset);
            if let Some(data) = user_objects.get(&key) {
                if data.len() as u64 == size {
                    info!("Mock: Read data for user {} from virtual offset {} with size {}", 
                          user_id, offset, size);
                    return Ok(data.clone());
                }
            }
        }
        
        Err(ErrorNotFound(format!(
            "Data not found for user {} at offset {} with size {}", 
            user_id, offset, size
        )))
    }
    
    fn log_deletion(&self, user_id: &str, _bucket: &str, key: &str, offset_size_list: &[(u64, u64)]) -> Result<(), Error> {
        // Mock implementation - just log the deletion
        info!("Mock: Logged deletion for user {} key {} with {} chunks", 
              user_id, key, offset_size_list.len());
        Ok(())
    }
    
    fn put_object(&self, user_id: &str, object_id: &str, data: &[u8]) -> Result<(), Error> {
        let mut store = self.data.lock().unwrap();
        let user_objects = store.entry(user_id.to_string()).or_insert_with(HashMap::new);
        user_objects.insert(object_id.to_string(), data.to_vec());
        
        info!("Mock: Stored object {} for user {} ({} bytes)", 
              object_id, user_id, data.len());
        
        Ok(())
    }
    
    fn get_object(&self, user_id: &str, object_id: &str) -> Result<Vec<u8>, Error> {
        let store = self.data.lock().unwrap();
        
        if let Some(user_objects) = store.get(user_id) {
            if let Some(data) = user_objects.get(object_id) {
                info!("Mock: Retrieved object {} for user {} ({} bytes)", 
                      object_id, user_id, data.len());
                return Ok(data.clone());
            }
        }
        
        Err(ErrorNotFound(format!(
            "Object {} not found for user {}", 
            object_id, user_id
        )))
    }
    
    fn delete_object(&self, user_id: &str, object_id: &str) -> Result<(), Error> {
        let mut store = self.data.lock().unwrap();
        
        if let Some(user_objects) = store.get_mut(user_id) {
            if user_objects.remove(object_id).is_some() {
                info!("Mock: Deleted object {} for user {}", object_id, user_id);
                
                // Clean up empty user entries
                if user_objects.is_empty() {
                    store.remove(user_id);
                }
                
                return Ok(());
            }
        }
        
        Err(ErrorNotFound(format!(
            "Object {} not found for user {}", 
            object_id, user_id
        )))
    }
    
    fn verify_object(&self, user_id: &str, object_id: &str, checksum: &[u8]) -> Result<bool, Error> {
        // Get the object data
        let data = self.get_object(user_id, object_id)?;
        
        // Simple checksum verification using basic hash
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        
        let mut hasher = DefaultHasher::new();
        data.hash(&mut hasher);
        let calculated_hash = hasher.finish().to_be_bytes();
        
        // Compare checksums
        let matches = calculated_hash.as_slice() == checksum;
        
        info!("Mock: Verified object {} for user {}: checksum matches = {}", 
              object_id, user_id, matches);
        
        Ok(matches)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::hash::{Hash, Hasher};

    #[test]
    fn test_mock_binary_store_basic_operations() {
        let store = MockBinaryStore::new();
        let user_id = "test_user_mock";
        let object_id = "test_object_mock";
        let test_data = b"Hello, Mock Storage!";
        
        // Initially empty
        assert_eq!(store.user_count(), 0);
        assert_eq!(store.object_count(user_id), 0);
        assert!(!store.user_exists(user_id));
        
        // Test put_object
        store.put_object(user_id, object_id, test_data).unwrap();
        
        // Verify counts and existence
        assert_eq!(store.user_count(), 1);
        assert_eq!(store.object_count(user_id), 1);
        assert!(store.user_exists(user_id));
        
        // Test list_objects
        let objects = store.list_objects(user_id);
        assert_eq!(objects.len(), 1);
        assert!(objects.contains(&object_id.to_string()));
        
        // Test get_object
        let retrieved_data = store.get_object(user_id, object_id).unwrap();
        assert_eq!(retrieved_data, test_data);
        
        // Test verify_object
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        test_data.hash(&mut hasher);
        let checksum = hasher.finish().to_be_bytes();
        assert!(store.verify_object(user_id, object_id, &checksum).unwrap());
        
        // Test with wrong checksum
        let wrong_checksum = [0u8; 8];
        assert!(!store.verify_object(user_id, object_id, &wrong_checksum).unwrap());
        
        // Test delete_object
        store.delete_object(user_id, object_id).unwrap();
        
        // After deletion, counts should be reset
        assert_eq!(store.user_count(), 0);
        assert_eq!(store.object_count(user_id), 0);
        assert!(!store.user_exists(user_id));
        
        // Clear test
        store.put_object(user_id, object_id, test_data).unwrap();
        assert_eq!(store.user_count(), 1);
        store.clear();
        assert_eq!(store.user_count(), 0);
    }
    
    #[test]
    fn test_mock_binary_store_error_cases() {
        let store = MockBinaryStore::new();
        let user_id = "test_user_error";
        let object_id = "nonexistent_object";
        
        // Test get_object for nonexistent object
        assert!(store.get_object(user_id, object_id).is_err());
        
        // Test delete_object for nonexistent object
        assert!(store.delete_object(user_id, object_id).is_err());
        
        // Test verify_object for nonexistent object
        let dummy_checksum = [0u8; 8];
        assert!(store.verify_object(user_id, object_id, &dummy_checksum).is_err());
        
        // Test list_objects for nonexistent user
        let objects = store.list_objects(user_id);
        assert!(objects.is_empty());
    }
    
    #[test]
    fn test_mock_binary_store_multiple_users_and_objects() {
        let store = MockBinaryStore::new();
        
        let user1 = "user1";
        let user2 = "user2";
        let obj1 = "object1";
        let obj2 = "object2";
        let data1 = b"data for object 1";
        let data2 = b"data for object 2";
        
        // Store objects for different users
        store.put_object(user1, obj1, data1).unwrap();
        store.put_object(user1, obj2, data2).unwrap();
        store.put_object(user2, obj1, data1).unwrap();
        
        // Verify counts
        assert_eq!(store.user_count(), 2);
        assert_eq!(store.object_count(user1), 2);
        assert_eq!(store.object_count(user2), 1);
        
        // Verify isolation
        assert_eq!(store.get_object(user1, obj1).unwrap(), data1);
        assert_eq!(store.get_object(user1, obj2).unwrap(), data2);
        assert_eq!(store.get_object(user2, obj1).unwrap(), data1);
        assert!(store.get_object(user2, obj2).is_err());
        
        // Test deletion isolation
        store.delete_object(user1, obj1).unwrap();
        assert_eq!(store.object_count(user1), 1);
        assert_eq!(store.object_count(user2), 1);
        assert!(store.get_object(user1, obj1).is_err());
        assert!(store.get_object(user2, obj1).is_ok());
    }
}