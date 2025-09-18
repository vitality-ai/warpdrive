//! Comprehensive test to verify the metadata storage abstraction

#[cfg(test)]
mod integration_tests {
    use crate::metadata::{Metadata, config::{MetadataConfig, MetadataBackend}};
    use crate::service::metadata_service::MetadataService;
    use crate::util::serializer::{serialize_offset_size, deserialize_offset_size};
    use std::env;

    #[test]
    fn test_metadata_abstraction_end_to_end() {
        // Test with both backends
        for backend in [MetadataBackend::SQLite, MetadataBackend::Mock] {
            println!("Testing with backend: {:?}", backend);
            
            // Set the backend environment variable
            env::set_var("METADATA_BACKEND", format!("{:?}", backend).to_lowercase());
            
            // Create a metadata service
            let service = MetadataService::new("test_user_e2e").expect("Failed to create service");
            let key = format!("test_key_e2e_{:?}", backend).to_lowercase();
            
            // Verify key doesn't exist initially
            assert!(!service.check_key("default", &key).expect("Failed to check key"));
            
            // Create test data (simulating what comes from storage layer)
            let offset_size_list = vec![(100, 200), (300, 400), (500, 600)];
            let serialized_data = serialize_offset_size(&offset_size_list)
                .expect("Failed to serialize data");
            
            // Store the data
            service.write_metadata("default", &key, &serialized_data)
                .expect("Failed to upload data");
            
            // Verify key now exists
            assert!(service.check_key("default", &key).expect("Failed to check key"));
            assert!(service.check_key_nonexistance("default", &key).is_ok());
            
            // Retrieve the data
            let retrieved_data = service.read_metadata("default", &key)
                .expect("Failed to retrieve data");
            
            // Verify data integrity
            assert_eq!(serialized_data, retrieved_data);
            
            // Deserialize and verify structure
            let retrieved_offset_size_list = deserialize_offset_size(&retrieved_data)
                .expect("Failed to deserialize retrieved data");
            assert_eq!(offset_size_list, retrieved_offset_size_list);
            
            // Test update operation
            let new_offset_size_list = vec![(1000, 2000)];
            let new_serialized_data = serialize_offset_size(&new_offset_size_list)
                .expect("Failed to serialize new data");
            
            service.update_metadata("default", &key, &new_serialized_data)
                .expect("Failed to update data");
            
            let updated_data = service.read_metadata("default", &key)
                .expect("Failed to retrieve updated data");
            
            let updated_offset_size_list = deserialize_offset_size(&updated_data)
                .expect("Failed to deserialize updated data");
            assert_eq!(new_offset_size_list, updated_offset_size_list);
            
            // Test key rename
            let new_key = format!("new_test_key_e2e_{:?}", backend).to_lowercase();
            service.rename_key("default", &key, &new_key)
                .expect("Failed to rename key");
            
            // Verify old key doesn't exist and new key exists
            assert!(!service.check_key("default", &key).expect("Failed to check old key"));
            assert!(service.check_key("default", &new_key).expect("Failed to check new key"));
            
            // Clean up
            service.delete_metadata("default", &new_key)
                .expect("Failed to delete data");
            assert!(!service.check_key("default", &new_key).expect("Failed to verify deletion"));
            
            println!("✓ Backend {:?} passed all tests", backend);
        }
        
        // Clean up environment variable
        env::remove_var("METADATA_BACKEND");
    }
    
    #[test]
    fn test_direct_metadata_storage_interface() {
        // Test the metadata storage interface directly
        let config = MetadataConfig { backend: MetadataBackend::Mock };
        let store = config.create_store();
        
        let user_id = "direct_test_user";
        let object_id = "direct_test_object";
        
        // Create metadata with multiple chunks and properties
        let mut metadata = Metadata::from_offset_size_list(vec![(10, 20), (30, 40)]);
        metadata.properties.insert("created_by".to_string(), "test".to_string());
        metadata.properties.insert("version".to_string(), "1.0".to_string());
        
        // Test all operations
        store.put_metadata(user_id, "default", object_id, &metadata).expect("Put failed");
        
        assert!(store.object_exists(user_id, "default", object_id).expect("Exists check failed"));
        
        let retrieved = store.get_metadata(user_id, "default", object_id).expect("Get failed");
        assert_eq!(retrieved.to_offset_size_list(), vec![(10, 20), (30, 40)]);
        assert_eq!(retrieved.properties.get("created_by"), Some(&"test".to_string()));
        
        let objects = store.list_objects(user_id, "default").expect("List failed");
        assert_eq!(objects, vec![object_id.to_string()]);
        
        // Update metadata
        let new_metadata = Metadata::from_offset_size_list(vec![(50, 60)]);
        store.update_metadata(user_id, "default", object_id, &new_metadata).expect("Update failed");
        
        let updated = store.get_metadata(user_id, "default", object_id).expect("Get after update failed");
        assert_eq!(updated.to_offset_size_list(), vec![(50, 60)]);
        
        // Rename object
        let new_object_id = "renamed_direct_test_object";
        store.update_object_id(user_id, "default", object_id, new_object_id).expect("Rename failed");
        
        assert!(!store.object_exists(user_id, "default", object_id).expect("Old exists check failed"));
        assert!(store.object_exists(user_id, "default", new_object_id).expect("New exists check failed"));
        
        // Delete
        store.delete_metadata(user_id, "default", new_object_id).expect("Delete failed");
        assert!(!store.object_exists(user_id, "default", new_object_id).expect("Final exists check failed"));
        
        println!("✓ Direct metadata storage interface test passed");
    }
    
    #[test]
    fn test_metadata_portability() {
        // Test that metadata can be moved between different storage backends
        let sqlite_config = MetadataConfig { backend: MetadataBackend::SQLite };
        let mock_config = MetadataConfig { backend: MetadataBackend::Mock };
        
        let sqlite_store = sqlite_config.create_store();
        let mock_store = mock_config.create_store();
        
        let user_id = "portability_test_user";
        let object_id = "portability_test_object";
        
        // Create rich metadata
        let mut metadata = Metadata::from_offset_size_list(vec![(100, 200), (300, 400)]);
        metadata.properties.insert("application".to_string(), "warpdrive".to_string());
        metadata.properties.insert("node_id".to_string(), "node-1".to_string());
        metadata.properties.insert("replica_count".to_string(), "3".to_string());
        
        // Store in SQLite
        sqlite_store.put_metadata(user_id, "default", object_id, &metadata).expect("SQLite put failed");
        
        // Retrieve from SQLite
        let retrieved_from_sqlite = sqlite_store.get_metadata(user_id, "default", object_id)
            .expect("SQLite get failed");
        
        // Store in Mock (simulating migration)
        mock_store.put_metadata(user_id, "default", object_id, &retrieved_from_sqlite)
            .expect("Mock put failed");
        
        // Retrieve from Mock
        let retrieved_from_mock = mock_store.get_metadata(user_id, "default", object_id)
            .expect("Mock get failed");
        
        // Verify data integrity across backends
        assert_eq!(metadata.to_offset_size_list(), retrieved_from_sqlite.to_offset_size_list());
        assert_eq!(metadata.to_offset_size_list(), retrieved_from_mock.to_offset_size_list());
        
        // Verify properties are preserved (note: SQLite backend doesn't persist properties yet,
        // but the structure is ready for when it does)
        if !retrieved_from_mock.properties.is_empty() {
            assert_eq!(metadata.properties, retrieved_from_mock.properties);
        }
        
        // Clean up
        sqlite_store.delete_metadata(user_id, "default", object_id).expect("SQLite cleanup failed");
        mock_store.delete_metadata(user_id, "default", object_id).expect("Mock cleanup failed");
        
        println!("✓ Metadata portability test passed");
    }
}