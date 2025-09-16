//! Mock binary storage implementation for testing

use crate::binary::BinaryStorage;
use actix_web::Error;
use actix_web::error::ErrorNotFound;
use log::info;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// A mock binary storage implementation that stores data in memory
/// Useful for testing without disk I/O operations
pub struct MockBinaryStore {
    /// In-memory storage: user_id -> list of stored data chunks
    storage: Arc<Mutex<HashMap<String, Vec<Vec<u8>>>>>,
    /// Deletion log: user_id -> list of deleted objects
    deletion_log: Arc<Mutex<HashMap<String, Vec<(String, Vec<(u64, u64)>)>>>>,
}

impl MockBinaryStore {
    pub fn new() -> Self {
        Self {
            storage: Arc::new(Mutex::new(HashMap::new())),
            deletion_log: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Clear all stored data (useful for testing)
    pub fn clear(&self) {
        self.storage.lock().unwrap().clear();
        self.deletion_log.lock().unwrap().clear();
    }

    /// Get the number of stored objects for a user (useful for testing)
    pub fn get_object_count(&self, user_id: &str) -> usize {
        self.storage
            .lock()
            .unwrap()
            .get(user_id)
            .map(|data| data.len())
            .unwrap_or(0)
    }

    /// Get deletion log entries for a user (useful for testing)
    pub fn get_deletion_log(&self, user_id: &str) -> Vec<(String, Vec<(u64, u64)>)> {
        self.deletion_log
            .lock()
            .unwrap()
            .get(user_id)
            .cloned()
            .unwrap_or_default()
    }
}

impl BinaryStorage for MockBinaryStore {
    fn put_object(&self, user_id: &str, _object_id: &str, data: &[u8]) -> Result<(u64, u64), Error> {
        let mut storage = self.storage.lock().unwrap();
        let user_data = storage.entry(user_id.to_string()).or_insert_with(Vec::new);
        
        let offset = user_data.len() as u64; // Use index as offset for simplicity
        let size = data.len() as u64;
        
        user_data.push(data.to_vec());
        
        info!("Mock: Stored object at offset {} with size {}", offset, size);
        Ok((offset, size))
    }

    fn get_object(&self, user_id: &str, _object_id: &str, offset: u64, size: u64) -> Result<Vec<u8>, Error> {
        let storage = self.storage.lock().unwrap();
        let user_data = storage.get(user_id).ok_or_else(|| {
            ErrorNotFound(format!("No data found for user: {}", user_id))
        })?;

        let index = offset as usize; // Use offset as index for simplicity
        if index >= user_data.len() {
            return Err(ErrorNotFound(format!("Invalid offset: {}", offset)));
        }

        let stored_data = &user_data[index];
        if stored_data.len() != size as usize {
            return Err(ErrorNotFound(format!(
                "Size mismatch: expected {}, found {}", 
                size, 
                stored_data.len()
            )));
        }

        info!("Mock: Retrieved object at offset {} with size {}", offset, size);
        Ok(stored_data.clone())
    }

    fn delete_object(&self, user_id: &str, object_id: &str, offset_size_list: &[(u64, u64)]) -> Result<(), Error> {
        let mut deletion_log = self.deletion_log.lock().unwrap();
        let user_log = deletion_log.entry(user_id.to_string()).or_insert_with(Vec::new);
        
        user_log.push((object_id.to_string(), offset_size_list.to_vec()));
        
        info!("Mock: Logged deletion for object: {}", object_id);
        Ok(())
    }

    fn verify_object(&self, _user_id: &str, _object_id: &str, checksum: &[u8]) -> Result<bool, Error> {
        // Mock implementation - just check if checksum is provided
        Ok(!checksum.is_empty())
    }

    fn put_objects_batch(&self, user_id: &str, data_list: Vec<&[u8]>) -> Result<Vec<(u64, u64)>, Error> {
        let mut offset_size_list = Vec::new();
        
        for data in data_list {
            let (offset, size) = self.put_object(user_id, "", data)?;
            offset_size_list.push((offset, size));
        }
        
        info!("Mock: Stored {} objects in batch", offset_size_list.len());
        Ok(offset_size_list)
    }

    fn get_objects_batch(&self, user_id: &str, offset_size_list: &[(u64, u64)]) -> Result<Vec<Vec<u8>>, Error> {
        let mut data_list = Vec::new();
        
        for &(offset, size) in offset_size_list {
            let data = self.get_object(user_id, "", offset, size)?;
            data_list.push(data);
        }
        
        info!("Mock: Retrieved {} objects in batch", data_list.len());
        Ok(data_list)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mock_binary_store_basic_operations() {
        let store = MockBinaryStore::new();
        let user_id = "test_user";
        let object_id = "test_object";
        let test_data = b"Hello, World!";

        // Test put_object
        let (offset, size) = store.put_object(user_id, object_id, test_data).unwrap();
        assert_eq!(size, test_data.len() as u64);
        assert_eq!(store.get_object_count(user_id), 1);

        // Test get_object
        let retrieved_data = store.get_object(user_id, object_id, offset, size).unwrap();
        assert_eq!(retrieved_data, test_data);

        // Test delete_object
        let offset_size_list = vec![(offset, size)];
        store.delete_object(user_id, object_id, &offset_size_list).unwrap();
        
        let deletion_log = store.get_deletion_log(user_id);
        assert_eq!(deletion_log.len(), 1);
        assert_eq!(deletion_log[0].0, object_id);
        assert_eq!(deletion_log[0].1, offset_size_list);
    }

    #[test]
    fn test_mock_binary_store_batch_operations() {
        let store = MockBinaryStore::new();
        let user_id = "test_user_batch";
        let test_data_list: Vec<&[u8]> = vec![b"data1", b"data2", b"data3"];

        // Test put_objects_batch
        let offset_size_list = store.put_objects_batch(user_id, test_data_list.clone()).unwrap();
        assert_eq!(offset_size_list.len(), 3);
        assert_eq!(store.get_object_count(user_id), 3);

        // Test get_objects_batch
        let retrieved_data_list = store.get_objects_batch(user_id, &offset_size_list).unwrap();
        assert_eq!(retrieved_data_list.len(), 3);
        
        for (i, data) in retrieved_data_list.iter().enumerate() {
            assert_eq!(data, test_data_list[i]);
        }
    }

    #[test]
    fn test_mock_binary_store_error_cases() {
        let store = MockBinaryStore::new();
        let user_id = "test_user_error";
        
        // Test get_object with non-existent user
        let result = store.get_object(user_id, "object", 0, 10);
        assert!(result.is_err());

        // Store some data first
        let test_data = b"test data";
        let (offset, size) = store.put_object(user_id, "object", test_data).unwrap();

        // Test get_object with invalid offset
        let result = store.get_object(user_id, "object", 999, size);
        assert!(result.is_err());

        // Test get_object with invalid size
        let result = store.get_object(user_id, "object", offset, 999);
        assert!(result.is_err());
    }

    #[test]
    fn test_mock_binary_store_verify_object() {
        let store = MockBinaryStore::new();
        
        // Test with empty checksum
        let result = store.verify_object("user", "object", &[]).unwrap();
        assert!(!result);

        // Test with non-empty checksum
        let result = store.verify_object("user", "object", &[1, 2, 3]).unwrap();
        assert!(result);
    }

    #[test]
    fn test_mock_binary_store_clear() {
        let store = MockBinaryStore::new();
        let user_id = "test_user_clear";
        let test_data = b"test data";

        // Store some data
        store.put_object(user_id, "object", test_data).unwrap();
        assert_eq!(store.get_object_count(user_id), 1);

        // Clear all data
        store.clear();
        assert_eq!(store.get_object_count(user_id), 0);
    }
}