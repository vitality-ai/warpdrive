//! Mock implementation of Storage trait for testing

use crate::storage::Storage;
use actix_web::Error;
use actix_web::error::ErrorNotFound;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use log::info;

/// Mock implementation of Storage for testing
pub struct MockBinaryStore {
    // In-memory storage: user_id -> bucket -> offset -> data
    data: Arc<Mutex<HashMap<String, HashMap<String, HashMap<u64, Vec<u8>>>>>>,
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
    
    /// Get the number of buckets for a specific user
    pub fn bucket_count(&self, user_id: &str) -> usize {
        let data = self.data.lock().unwrap();
        data.get(user_id).map(|buckets| buckets.len()).unwrap_or(0)
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
    
    /// List all buckets for a user
    pub fn list_buckets(&self, user_id: &str) -> Vec<String> {
        let data = self.data.lock().unwrap();
        data.get(user_id)
            .map(|buckets| buckets.keys().cloned().collect())
            .unwrap_or_default()
    }
}

impl Default for MockBinaryStore {
    fn default() -> Self {
        Self::new()
    }
}

impl Storage for MockBinaryStore {
    fn write(&self, user_id: &str, bucket: &str, data: &[u8]) -> Result<(u64, u64), Error> {
        let mut store = self.data.lock().unwrap();
        let user_entry = store.entry(user_id.to_string()).or_insert_with(HashMap::new);
        let bucket_entry = user_entry.entry(bucket.to_string()).or_insert_with(HashMap::new);
        let next_offset = bucket_entry.keys().copied().max().unwrap_or(0)
            + bucket_entry.get(&bucket_entry.keys().copied().max().unwrap_or(0)).map(|v| v.len() as u64).unwrap_or(0);
        let size = data.len() as u64;
        bucket_entry.insert(next_offset, data.to_vec());
        info!("Mock: Wrote data for user {} bucket {} at offset {} size {}", user_id, bucket, next_offset, size);
        Ok((next_offset, size))
    }

    fn read(&self, user_id: &str, bucket: &str, offset: u64, size: u64) -> Result<Vec<u8>, Error> {
        let store = self.data.lock().unwrap();
        if let Some(user_entry) = store.get(user_id) {
            if let Some(bucket_entry) = user_entry.get(bucket) {
                if let Some(data) = bucket_entry.get(&offset) {
                    if data.len() as u64 == size {
                        return Ok(data.clone());
                    }
                }
            }
        }
        Err(ErrorNotFound(format!("Data not found for user {} bucket {} at offset {} size {}", user_id, bucket, offset, size)))
    }

    fn delete(&self, user_id: &str, bucket: &str, offset_size_list: &[(u64, u64)]) -> Result<(), Error> {
        let mut store = self.data.lock().unwrap();
        if let Some(user_entry) = store.get_mut(user_id) {
            if let Some(bucket_entry) = user_entry.get_mut(bucket) {
                for (offset, size) in offset_size_list {
                    if let Some(data) = bucket_entry.get(offset) {
                        if data.len() as u64 == *size {
                            bucket_entry.remove(offset);
                        }
                    }
                }
                if bucket_entry.is_empty() {
                    user_entry.remove(bucket);
                }
            }
            if user_entry.is_empty() {
                store.remove(user_id);
            }
        }
        info!("Mock: Deleted {} ranges for user {} bucket {}", offset_size_list.len(), user_id, bucket);
        Ok(())
    }

    fn verify(&self, user_id: &str, bucket: &str, offset: u64, size: u64, checksum: &[u8]) -> Result<bool, Error> {
        let data = self.read(user_id, bucket, offset, size)?;
        use sha2::{Sha256, Digest};
        let mut hasher = Sha256::new();
        hasher.update(&data);
        let calculated = hasher.finalize();
        Ok(calculated.as_slice() == checksum)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mock_binary_store_basic_operations() {
        let store = MockBinaryStore::new();
        let user_id = "test_user_mock";
        let bucket = "test_bucket";
        let test_data = b"Hello, Mock Storage!";
        
        // Initially empty
        assert_eq!(store.user_count(), 0);
        assert_eq!(store.bucket_count(user_id), 0);
        assert!(!store.user_exists(user_id));
        
        // Test write
        let (offset, size) = store.write(user_id, bucket, test_data).unwrap();
        
        // Verify counts and existence
        assert_eq!(store.user_count(), 1);
        assert_eq!(store.bucket_count(user_id), 1);
        assert!(store.user_exists(user_id));
        
        // Test list_buckets
        let buckets = store.list_buckets(user_id);
        assert_eq!(buckets.len(), 1);
        assert!(buckets.contains(&bucket.to_string()));
        
        // Test read
        let retrieved_data = store.read(user_id, bucket, offset, size).unwrap();
        assert_eq!(retrieved_data, test_data);
        
        // Test verify (SHA-256)
        use sha2::{Sha256, Digest};
        let mut hasher = Sha256::new();
        hasher.update(&test_data[..]);
        let checksum = hasher.finalize().to_vec();
        assert!(store.verify(user_id, bucket, offset, size, &checksum).unwrap());
        
        // Test with wrong checksum
        let wrong_checksum = [0u8; 8];
        assert!(!store.verify(user_id, bucket, offset, size, &wrong_checksum).unwrap());
        
        // Test delete
        store.delete(user_id, bucket, &[(offset, size)]).unwrap();
        
        // After deletion, counts should be reset
        assert_eq!(store.user_count(), 0);
        assert_eq!(store.bucket_count(user_id), 0);
        assert!(!store.user_exists(user_id));
        
        // Clear test
        let _ = store.write(user_id, bucket, test_data).unwrap();
        assert_eq!(store.user_count(), 1);
        store.clear();
        assert_eq!(store.user_count(), 0);
    }
    
    #[test]
    fn test_mock_binary_store_error_cases() {
        let store = MockBinaryStore::new();
        let user_id = "test_user_error";
        let bucket = "test_bucket";
        
        // Test read for nonexistent data
        assert!(store.read(user_id, bucket, 0, 1).is_err());
        
        // Test verify for nonexistent data
        let dummy_checksum = [0u8; 32];
        assert!(store.verify(user_id, bucket, 0, 1, &dummy_checksum).is_err());
        
        // Test list_buckets for nonexistent user
        let buckets = store.list_buckets(user_id);
        assert!(buckets.is_empty());
    }
    
    #[test]
    fn test_mock_binary_store_multiple_users_and_objects() {
        let store = MockBinaryStore::new();
        
        let user1 = "user1";
        let user2 = "user2";
        let bucket1 = "bucket1";
        let bucket2 = "bucket2";
        let data1 = b"data for bucket 1";
        let data2 = b"data for bucket 2";
        
        // Store chunks for different users/buckets
        let _ = store.write(user1, bucket1, data1).unwrap();
        let _ = store.write(user1, bucket2, data2).unwrap();
        let _ = store.write(user2, bucket1, data1).unwrap();
        
        // Verify counts
        assert_eq!(store.user_count(), 2);
        assert_eq!(store.bucket_count(user1), 2);
        assert_eq!(store.bucket_count(user2), 1);
        
        // Basic reads work
        // (We don't track exact offsets here; just ensure write/read pairs work)
        let (o1, s1) = store.write(user1, bucket1, data1).unwrap();
        assert_eq!(store.read(user1, bucket1, o1, s1).unwrap(), data1);
    }
}