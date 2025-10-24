//! Mock implementation of Storage trait for testing

use crate::storage::{Storage, ObjectId};
use actix_web::Error;
use actix_web::error::{ErrorNotFound, ErrorBadRequest};
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
    
    /// Check if an object exists for a user
    pub fn object_exists(&self, user_id: &str, key: &str) -> bool {
        let data = self.data.lock().unwrap();
        data.get(user_id)
            .map(|objects| objects.contains_key(key))
            .unwrap_or(false)
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
    
    fn delete_object(&self, user_id: &str, _bucket: &str, key: &str, offset_size_list: &[(u64, u64)]) -> Result<(), Error> {
        // Check if user exists
        if !self.user_exists(user_id) {
            return Err(ErrorBadRequest("User does not exist"));
        }
        
        // Mock implementation - just log the deletion
        // We don't check object existence since the Storage trait is low-level
        info!("Mock: Deleted object for user {} key {} with {} chunks", 
              user_id, key, offset_size_list.len());
        Ok(())
    }
    
    
    fn verify_object(&self, user_id: &str, bucket: &str, key: &str, checksum: &[u8]) -> Result<bool, Error> {
        // Check if user exists
        if !self.user_exists(user_id) {
            return Err(ErrorBadRequest("User does not exist"));
        }
        
        // For mock, we'll simulate checksum verification by checking if it's not all zeros
        // We don't check object existence since the Storage trait is low-level
        let is_valid = !checksum.iter().all(|&x| x == 0);
        
        info!("Mock: Verified object {} for user {} bucket {}: checksum verification = {}", 
              key, user_id, bucket, is_valid);
        
        Ok(is_valid)
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
        
        // Test write_data
        let (offset, size) = store.write_data(user_id, "test_bucket", test_data).unwrap();
        assert_eq!(size, test_data.len() as u64);
        
        // Test read_data
        let retrieved_data = store.read_data(user_id, "test_bucket", offset, size).unwrap();
        assert_eq!(retrieved_data, test_data);
        
        // Test verify_object
        use md5;
        let checksum = md5::compute(test_data);
        assert!(store.verify_object(user_id, "test_bucket", object_id, checksum.as_slice()).unwrap());
        
        // Test with wrong checksum
        let wrong_checksum = [0u8; 16];
        assert!(!store.verify_object(user_id, "test_bucket", object_id, &wrong_checksum).unwrap());
        
        // Test delete_object
        store.delete_object(user_id, "test_bucket", object_id, &[(offset, size)]).unwrap();
    }
    
    #[test]
    fn test_mock_binary_store_error_cases() {
        let store = MockBinaryStore::new();
        let user_id = "test_user_error";
        let bucket = "test_bucket";
        
        // Test read_data for nonexistent data
        assert!(store.read_data(user_id, bucket, 0, 0).is_err());
        
        // Test delete_object for nonexistent object
        assert!(store.delete_object(user_id, bucket, "nonexistent_key", &[]).is_err());
        
        // Test verify_object for nonexistent object
        let dummy_checksum = [0u8; 16];
        assert!(store.verify_object(user_id, bucket, "nonexistent_key", &dummy_checksum).is_err());
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
        
        // Store data for different users
        let bucket = "test_bucket";
        let (offset1, size1) = store.write_data(user1, bucket, data1).unwrap();
        let (offset2, size2) = store.write_data(user1, bucket, data2).unwrap();
        let (offset3, size3) = store.write_data(user2, bucket, data1).unwrap();
        
        // Verify data can be read back
        assert_eq!(store.read_data(user1, bucket, offset1, size1).unwrap(), data1);
        assert_eq!(store.read_data(user1, bucket, offset2, size2).unwrap(), data2);
        assert_eq!(store.read_data(user2, bucket, offset3, size3).unwrap(), data1);
        
        // Test deletion
        store.delete_object(user1, bucket, obj1, &[(offset1, size1)]).unwrap();
    }
}