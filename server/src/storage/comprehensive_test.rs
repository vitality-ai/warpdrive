//! Comprehensive tests for the storage abstraction layer

#[cfg(test)]
mod integration_tests {
    use crate::storage::config::{StorageConfig, StorageBackend};
    use std::hash::{Hash, Hasher};

    #[test]
    fn test_direct_storage_interface() {
        // Test the storage interface directly
        let config = StorageConfig { backend: StorageBackend::Mock };
        let store = config.create_store();
        
        let user_id = "direct_test_user";
        let object_id = "direct_test_object";
        let test_data = b"Direct storage interface test data";
        
        // Test all operations
        store.put_object(user_id, object_id, test_data).expect("Put failed");
        
        let retrieved = store.get_object(user_id, object_id).expect("Get failed");
        assert_eq!(retrieved, test_data);
        
        // Test verification
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        test_data.hash(&mut hasher);
        let checksum = hasher.finish().to_be_bytes();
        assert!(store.verify_object(user_id, object_id, &checksum).expect("Verify failed"));
        
        // Test with wrong checksum
        let wrong_checksum = [0u8; 8];
        assert!(!store.verify_object(user_id, object_id, &wrong_checksum).expect("Verify failed"));
        
        store.delete_object(user_id, object_id).expect("Delete failed");
        assert!(store.get_object(user_id, object_id).is_err());
    }
    
    #[test]
    fn test_storage_portability() {
        // Test that data can be moved between different storage backends
        // Note: This is conceptual since LocalXFS uses files and Mock uses memory
        let mock_config = StorageConfig { backend: StorageBackend::Mock };
        let mock_store = mock_config.create_store();
        
        let user_id = "portability_test_user";
        let object_id = "portability_test_object";
        let test_data = b"Portability test data across backends";
        
        // Store in Mock
        mock_store.put_object(user_id, object_id, test_data).expect("Mock put failed");
        
        // Retrieve from Mock
        let retrieved_from_mock = mock_store.get_object(user_id, object_id)
            .expect("Mock get failed");
        
        // Verify data integrity
        assert_eq!(test_data, retrieved_from_mock.as_slice());
        
        // Test verification works across operations
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        test_data.hash(&mut hasher);
        let checksum = hasher.finish().to_be_bytes();
        assert!(mock_store.verify_object(user_id, object_id, &checksum).expect("Verify failed"));
    }
    
    #[test]
    fn test_storage_abstraction_end_to_end() {
        // Test the complete storage abstraction
        let backends = vec![StorageBackend::Mock, StorageBackend::LocalXFS];
        
        for backend in backends {
            let config = StorageConfig { backend: backend.clone() };
            let store = config.create_store();
            
            let user_id = format!("e2e_user_{:?}", backend);
            let object_id = "e2e_object";
            let test_data = format!("End-to-end test data for {:?}", backend).into_bytes();
            
            // Test complete flow
            store.put_object(&user_id, object_id, &test_data).expect("Put failed");
            
            let retrieved = store.get_object(&user_id, object_id).expect("Get failed");
            assert_eq!(retrieved, test_data);
            
            // Test verification
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            test_data.hash(&mut hasher);
            let checksum = hasher.finish().to_be_bytes();
            assert!(store.verify_object(&user_id, object_id, &checksum).expect("Verify failed"));
            
            store.delete_object(&user_id, object_id).expect("Delete failed");
            assert!(store.get_object(&user_id, object_id).is_err());
        }
    }
    
    #[test]
    fn test_concurrent_operations() {
        use std::sync::Arc;
        use std::thread;
        
        let config = StorageConfig { backend: StorageBackend::Mock };
        let store = config.create_store();
        
        let store_clone = Arc::clone(&store);
        let handles: Vec<_> = (0..5).map(|i| {
            let store = Arc::clone(&store_clone);
            thread::spawn(move || {
                let user_id = format!("concurrent_user_{}", i);
                let object_id = "concurrent_object";
                let test_data = format!("Concurrent test data {}", i).into_bytes();
                
                store.put_object(&user_id, object_id, &test_data).unwrap();
                let retrieved = store.get_object(&user_id, object_id).unwrap();
                assert_eq!(retrieved, test_data);
                store.delete_object(&user_id, object_id).unwrap();
            })
        }).collect();
        
        for handle in handles {
            handle.join().unwrap();
        }
    }
    
    #[test]
    fn test_large_data_handling() {
        let config = StorageConfig { backend: StorageBackend::Mock };
        let store = config.create_store();
        
        let user_id = "large_data_user";
        let object_id = "large_object";
        
        // Create a large data blob (1MB)
        let large_data: Vec<u8> = (0..1024*1024).map(|i| (i % 256) as u8).collect();
        
        store.put_object(user_id, object_id, &large_data).expect("Large put failed");
        
        let retrieved = store.get_object(user_id, object_id).expect("Large get failed");
        assert_eq!(retrieved, large_data);
        
        // Test verification with large data
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        large_data.hash(&mut hasher);
        let checksum = hasher.finish().to_be_bytes();
        assert!(store.verify_object(user_id, object_id, &checksum).expect("Large verify failed"));
        
        store.delete_object(user_id, object_id).expect("Large delete failed");
    }
    
    #[test]
    fn test_storage_error_scenarios() {
        let config = StorageConfig { backend: StorageBackend::Mock };
        let store = config.create_store();
        
        let user_id = "error_test_user";
        let nonexistent_object = "nonexistent_object";
        
        // Test get for nonexistent object
        assert!(store.get_object(user_id, nonexistent_object).is_err());
        
        // Test delete for nonexistent object
        assert!(store.delete_object(user_id, nonexistent_object).is_err());
        
        // Test verify for nonexistent object
        let dummy_checksum = [0u8; 8];
        assert!(store.verify_object(user_id, nonexistent_object, &dummy_checksum).is_err());
        
        // Test empty data
        let empty_object = "empty_object";
        store.put_object(user_id, empty_object, &[]).expect("Empty put failed");
        let retrieved = store.get_object(user_id, empty_object).expect("Empty get failed");
        assert!(retrieved.is_empty());
        
        store.delete_object(user_id, empty_object).expect("Empty delete failed");
    }
}