//! Comprehensive tests for binary storage implementations

#[cfg(test)]
mod integration_tests {
    use crate::binary::{BinaryStorage, config::{BinaryConfig, BinaryBackend}};
    use std::env;

    /// Test that different binary storage implementations provide equivalent functionality
    #[test]
    fn test_binary_storage_implementations_equivalence() {
        let test_cases = vec![
            BinaryBackend::LocalXFS,
            BinaryBackend::Mock,
        ];

        for backend in test_cases {
            let config = BinaryConfig { backend: backend.clone() };
            let store = config.create_store();
            
            let user_id = &format!("test_user_{:?}", backend);
            let object_id = "test_object";
            let test_data = b"Hello, Binary Storage!";

            // Test put_object
            let (offset, size) = store.put_object(user_id, object_id, test_data).unwrap();
            assert_eq!(size, test_data.len() as u64);

            // Test get_object
            let retrieved_data = store.get_object(user_id, object_id, offset, size).unwrap();
            assert_eq!(retrieved_data, test_data);

            // Test delete_object
            let offset_size_list = vec![(offset, size)];
            store.delete_object(user_id, object_id, &offset_size_list).unwrap();

            // Test verify_object
            let verification_result = store.verify_object(user_id, object_id, &[1, 2, 3]).unwrap();
            assert!(verification_result);
        }
    }

    /// Test batch operations across different implementations
    #[test]
    fn test_binary_storage_batch_operations() {
        let test_cases = vec![
            BinaryBackend::LocalXFS,
            BinaryBackend::Mock,
        ];

        for backend in test_cases {
            let config = BinaryConfig { backend: backend.clone() };
            let store = config.create_store();
            
            let user_id = &format!("test_user_batch_{:?}", backend);
            let test_data_list: Vec<&[u8]> = vec![
                b"first object",
                b"second object",
                b"third object",
            ];

            // Test put_objects_batch
            let offset_size_list = store.put_objects_batch(user_id, test_data_list.clone()).unwrap();
            assert_eq!(offset_size_list.len(), 3);

            // Test get_objects_batch
            let retrieved_data_list = store.get_objects_batch(user_id, &offset_size_list).unwrap();
            assert_eq!(retrieved_data_list.len(), 3);
            
            for (i, data) in retrieved_data_list.iter().enumerate() {
                assert_eq!(data, test_data_list[i]);
            }
        }
    }

    /// Test configuration-based storage backend selection
    #[test]
    fn test_configuration_based_backend_selection() {
        // Test LocalXFS backend
        env::set_var("BINARY_BACKEND", "localxfs");
        let config = BinaryConfig::from_env();
        assert_eq!(config.backend, BinaryBackend::LocalXFS);
        let _store = config.create_store();

        // Test Mock backend
        env::set_var("BINARY_BACKEND", "mock");
        let config = BinaryConfig::from_env();
        assert_eq!(config.backend, BinaryBackend::Mock);
        let _store = config.create_store();

        // Clean up environment
        env::remove_var("BINARY_BACKEND");
    }

    /// Test that binary storage abstraction allows switching implementations without code changes
    #[test]
    fn test_binary_storage_abstraction_portability() {
        fn use_binary_storage(store: &dyn BinaryStorage, user_id: &str) -> Result<(), actix_web::Error> {
            let object_id = "portable_test_object";
            let test_data = b"This test validates portability";

            // Store data
            let (offset, size) = store.put_object(user_id, object_id, test_data)?;
            
            // Retrieve data
            let retrieved_data = store.get_object(user_id, object_id, offset, size)?;
            assert_eq!(retrieved_data, test_data);
            
            // Delete data
            store.delete_object(user_id, object_id, &[(offset, size)])?;
            
            Ok(())
        }

        // Test with LocalXFS backend
        let config_xfs = BinaryConfig { backend: BinaryBackend::LocalXFS };
        let store_xfs = config_xfs.create_store();
        use_binary_storage(store_xfs.as_ref(), "portable_user_xfs").unwrap();

        // Test with Mock backend
        let config_mock = BinaryConfig { backend: BinaryBackend::Mock };
        let store_mock = config_mock.create_store();
        use_binary_storage(store_mock.as_ref(), "portable_user_mock").unwrap();
    }

    /// End-to-end test demonstrating the complete binary storage workflow
    #[test]
    fn test_binary_storage_abstraction_end_to_end() {
        // This test simulates how the binary storage would be used in the actual application
        let config = BinaryConfig::from_env();
        let store = config.create_store();
        
        let user_id = "e2e_test_user";
        let objects = vec![
            ("object1", b"Data for object 1" as &[u8]),
            ("object2", b"Data for object 2"),
            ("object3", b"Data for object 3"),
        ];

        // Store multiple objects
        let mut stored_metadata = Vec::new();
        for (object_id, data) in &objects {
            let (offset, size) = store.put_object(user_id, object_id, data).unwrap();
            stored_metadata.push((*object_id, offset, size));
        }

        // Retrieve all objects
        for (object_id, offset, size) in &stored_metadata {
            let retrieved_data = store.get_object(user_id, object_id, *offset, *size).unwrap();
            let original_data = objects.iter()
                .find(|(id, _)| id == object_id)
                .map(|(_, data)| *data)
                .unwrap();
            assert_eq!(retrieved_data, original_data);
        }

        // Delete some objects
        let to_delete = &stored_metadata[..2]; // Delete first two objects
        for (object_id, offset, size) in to_delete {
            store.delete_object(user_id, object_id, &[(*offset, *size)]).unwrap();
        }

        // Verify remaining object is still accessible
        let (remaining_object_id, offset, size) = &stored_metadata[2];
        let retrieved_data = store.get_object(user_id, remaining_object_id, *offset, *size).unwrap();
        assert_eq!(retrieved_data, objects[2].1);
    }
}