//! integration_tests.rs
//! 
//! Integration tests for the metadata storage abstraction layer.

#[cfg(test)]
mod integration_tests {
    use std::sync::Arc;
    use crate::metadata::{MetadataStorage, Metadata};
    use crate::config::{MetadataConfig, MetadataBackend};

    /// Test that both SQLite and Mock implementations work through the MetadataStorage trait
    #[test]
    fn test_metadata_storage_abstraction() {
        let test_data = vec![
            ("sqlite", MetadataConfig { backend: MetadataBackend::SQLite }),
            ("mock", MetadataConfig { backend: MetadataBackend::Mock }),
        ];

        for (backend_name, config) in test_data {
            println!("Testing {} backend", backend_name);
            
            let store = config.create_metadata_store();
            test_metadata_operations(store, backend_name);
        }
    }

    fn test_metadata_operations(store: Arc<dyn MetadataStorage>, backend_name: &str) {
        let user_id = format!("test_user_{}", backend_name);
        let object_id = format!("test_object_{}", backend_name);
        let metadata = Metadata::new(vec![(0, 100), (100, 200)]);

        // Test that object doesn't exist initially
        assert!(!store.exists(&user_id, &object_id).unwrap(), "{}: Object should not exist initially", backend_name);

        // Test put_metadata
        store.put_metadata(&user_id, &object_id, &metadata).unwrap();
        assert!(store.exists(&user_id, &object_id).unwrap(), "{}: Object should exist after put", backend_name);

        // Test get_metadata
        let retrieved = store.get_metadata(&user_id, &object_id).unwrap();
        assert_eq!(retrieved.offset_size_list, metadata.offset_size_list, "{}: Retrieved metadata should match", backend_name);

        // Test update_metadata
        let new_metadata = Metadata::new(vec![(0, 300), (300, 100)]);
        store.update_metadata(&user_id, &object_id, &new_metadata).unwrap();
        let updated = store.get_metadata(&user_id, &object_id).unwrap();
        assert_eq!(updated.offset_size_list, new_metadata.offset_size_list, "{}: Updated metadata should match", backend_name);

        // Test update_object_id
        let new_object_id = format!("new_test_object_{}", backend_name);
        store.update_object_id(&user_id, &object_id, &new_object_id).unwrap();
        assert!(!store.exists(&user_id, &object_id).unwrap(), "{}: Old object ID should not exist", backend_name);
        assert!(store.exists(&user_id, &new_object_id).unwrap(), "{}: New object ID should exist", backend_name);

        // Test list_objects
        let objects = store.list_objects(&user_id).unwrap();
        assert!(objects.contains(&new_object_id), "{}: List should contain new object ID", backend_name);

        // Test delete_metadata
        store.delete_metadata(&user_id, &new_object_id).unwrap();
        assert!(!store.exists(&user_id, &new_object_id).unwrap(), "{}: Object should not exist after delete", backend_name);
    }

    /// Test backward compatibility with the Database struct
    #[test]
    fn test_database_backward_compatibility() {
        use crate::database::Database;
        use crate::util::serializer::serialize_offset_size;

        let user = "test_user_compat";
        let key = "test_key_compat";
        let offset_size_list = vec![(0, 100), (100, 200)];
        let offset_size_bytes = serialize_offset_size(&offset_size_list).unwrap();

        let db = Database::new(user).unwrap();

        // Test that old Database API still works
        assert!(!db.check_key(key).unwrap());
        
        db.upload_sql(key, &offset_size_bytes).unwrap();
        assert!(db.check_key(key).unwrap());

        let retrieved_bytes = db.get_offset_size_lists(key).unwrap();
        assert_eq!(retrieved_bytes, offset_size_bytes);

        // Test update
        let new_offset_size_list = vec![(0, 300)];
        let new_offset_size_bytes = serialize_offset_size(&new_offset_size_list).unwrap();
        db.update_file_db(key, &new_offset_size_bytes).unwrap();

        let updated_bytes = db.get_offset_size_lists(key).unwrap();
        assert_eq!(updated_bytes, new_offset_size_bytes);

        // Test key update
        let new_key = "new_test_key_compat";
        db.update_key_from_db(key, new_key).unwrap();
        assert!(!db.check_key(key).unwrap());
        assert!(db.check_key(new_key).unwrap());

        // Test delete
        db.delete_from_db(new_key).unwrap();
        assert!(!db.check_key(new_key).unwrap());
    }
}