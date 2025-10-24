//! Comprehensive tests for the storage abstraction layer

#[cfg(test)]
mod integration_tests {
    use crate::storage::{Storage, mock_store::MockBinaryStore};
    use std::sync::Arc;

    #[test]
    fn test_direct_storage_interface() {
        // Test the storage interface directly
        let store = Arc::new(MockBinaryStore::new());
        
        let user_id = "direct_test_user";
        let object_id = "direct_test_object";
        let test_data = b"Direct storage interface test data";
        
        // Test all operations
        let bucket = "test_bucket";
        let (offset, size) = store.write_data(user_id, bucket, test_data).expect("Write failed");
        
        let retrieved = store.read_data(user_id, bucket, offset, size).expect("Read failed");
        assert_eq!(retrieved, test_data);
        
        // Test verification
        use md5;
        let checksum = md5::compute(test_data);
        assert!(store.verify_object(user_id, bucket, object_id, checksum.as_slice()).expect("Verify failed"));
        
        // Test with wrong checksum
        let wrong_checksum = [0u8; 16];
        assert!(!store.verify_object(user_id, bucket, object_id, &wrong_checksum).expect("Verify failed"));
        
        store.delete_object(user_id, bucket, object_id, &[(offset, size)]).expect("Delete failed");
    }
    
    #[test]
    fn test_storage_portability() {
        // Test that data can be moved between different storage backends
        // Note: This is conceptual since LocalXFS uses files and Mock uses memory
        let mock_store = Arc::new(MockBinaryStore::new());
        
        let user_id = "portability_test_user";
        let object_id = "portability_test_object";
        let test_data = b"Portability test data across backends";
        
        // Store in Mock
        let bucket = "test_bucket";
        let (offset, size) = mock_store.write_data(user_id, bucket, test_data).expect("Mock write failed");
        
        // Retrieve from Mock
        let retrieved_from_mock = mock_store.read_data(user_id, bucket, offset, size)
            .expect("Mock read failed");
        
        // Verify data integrity
        assert_eq!(test_data, retrieved_from_mock.as_slice());
        
        // Test verification works across operations
        use md5;
        let checksum = md5::compute(test_data);
        assert!(mock_store.verify_object(user_id, bucket, object_id, checksum.as_slice()).expect("Verify failed"));
    }
    
    #[test]
    fn test_storage_abstraction_end_to_end() {
        // Test the complete storage abstraction
        let store = Arc::new(MockBinaryStore::new());
        
        let user_id = "e2e_user";
        let object_id = "e2e_object";
        let test_data = b"End-to-end test data".to_vec();
            
            // Test complete flow
            let bucket = "test_bucket";
            let (offset, size) = store.write_data(&user_id, bucket, &test_data).expect("Write failed");
            
            let retrieved = store.read_data(&user_id, bucket, offset, size).expect("Read failed");
            assert_eq!(retrieved, test_data);
            
            // Test verification
            use md5;
            let checksum = md5::compute(&test_data);
            assert!(store.verify_object(&user_id, bucket, object_id, checksum.as_slice()).expect("Verify failed"));
            
        store.delete_object(&user_id, bucket, object_id, &[(offset, size)]).expect("Delete failed");
    }
    
    #[test]
    fn test_concurrent_operations() {
        use std::sync::Arc;
        use std::thread;
        
        let store = Arc::new(MockBinaryStore::new());
        
        let store_clone = Arc::clone(&store);
        let handles: Vec<_> = (0..5).map(|i| {
            let store = Arc::clone(&store_clone);
            thread::spawn(move || {
                let user_id = format!("concurrent_user_{}", i);
                let object_id = "concurrent_object";
                let test_data = format!("Concurrent test data {}", i).into_bytes();
                
                let bucket = "test_bucket";
                let (offset, size) = store.write_data(&user_id, bucket, &test_data).unwrap();
                let retrieved = store.read_data(&user_id, bucket, offset, size).unwrap();
                assert_eq!(retrieved, test_data);
                store.delete_object(&user_id, bucket, object_id, &[(offset, size)]).unwrap();
            })
        }).collect();
        
        for handle in handles {
            handle.join().unwrap();
        }
    }
    
    #[test]
    fn test_large_data_handling() {
        let store = Arc::new(MockBinaryStore::new());
        
        let user_id = "large_data_user";
        let object_id = "large_object";
        
        // Create a large data blob (1MB)
        let large_data: Vec<u8> = (0..1024*1024).map(|i| (i % 256) as u8).collect();
        
        let bucket = "test_bucket";
        let (offset, size) = store.write_data(user_id, bucket, &large_data).expect("Large write failed");
        
        let retrieved = store.read_data(user_id, bucket, offset, size).expect("Large read failed");
        assert_eq!(retrieved, large_data);
        
        // Test verification with large data
        use md5;
        let checksum = md5::compute(&large_data);
        assert!(store.verify_object(user_id, bucket, object_id, checksum.as_slice()).expect("Large verify failed"));
        
        store.delete_object(user_id, bucket, object_id, &[(offset, size)]).expect("Large delete failed");
    }
    
    #[test]
    fn test_storage_error_scenarios() {
        let store = Arc::new(MockBinaryStore::new());
        
        let user_id = "error_test_user";
        let nonexistent_object = "nonexistent_object";
        
        let bucket = "test_bucket";
        
        // Test read for nonexistent data
        assert!(store.read_data(user_id, bucket, 0, 0).is_err());
        
        // Test delete for nonexistent object
        assert!(store.delete_object(user_id, bucket, nonexistent_object, &[]).is_err());
        
        // Test verify for nonexistent object
        let dummy_checksum = [0u8; 16];
        assert!(store.verify_object(user_id, bucket, nonexistent_object, &dummy_checksum).is_err());
        
        // Test empty data
        let empty_object = "empty_object";
        let (offset, size) = store.write_data(user_id, bucket, &[]).expect("Empty write failed");
        let retrieved = store.read_data(user_id, bucket, offset, size).expect("Empty read failed");
        assert!(retrieved.is_empty());
        
        store.delete_object(user_id, bucket, empty_object, &[(offset, size)]).expect("Empty delete failed");
    }
}