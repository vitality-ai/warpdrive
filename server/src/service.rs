//service.rs
use actix_web::{ web, HttpResponse,Error, HttpRequest};
use futures::StreamExt;
use bytes::BytesMut;
use log::{info, error, warn};
use actix_web::error::{ErrorInternalServerError,ErrorBadRequest};
use log_mdc;


use crate::storage::{write_files_to_storage, get_files_from_storage, delete_and_log};
use crate::database::Database;
use crate::util::serializer::{serialize_offset_size, deserialize_offset_size};


fn header_handler(req: HttpRequest) ->  Result<String, Error> {
    let user = req.headers()
        .get("User")
        .ok_or_else(|| ErrorBadRequest("Missing User header"))?
        .to_str()
        .map_err(|_| ErrorBadRequest("Invalid User header value"))?
        .to_string();
    
    log_mdc::insert("user", &user);    
    Ok(user)
}

pub async fn put_service(key: String, mut payload: web::Payload, req: HttpRequest) -> Result<HttpResponse, Error>{

    let user = header_handler(req)?;

    let db = Database::new(&user)?;
    if db.check_key(&key).map_err(ErrorInternalServerError)? {
        warn!("Key already exists: {}", key);
        return Ok(HttpResponse::BadRequest().body("Key already exists"));
    }

    info!("Starting chunk load");
    let mut bytes = BytesMut::new();
    while let Some(chunk) = payload.next().await {
        let chunk = chunk.map_err(ErrorInternalServerError)?;
        bytes.extend_from_slice(&chunk);
    }

    if bytes.is_empty() {
        error!("No data uploaded with key: {}", key);
        return Ok(HttpResponse::BadRequest().body("No data was uploaded"));
    }

    info!("Total received data size: {} bytes", bytes.len());

    let offset_size_list = write_files_to_storage(&user, &bytes)?;


    if offset_size_list.is_empty()  {
        error!("No data in data list with key: {}", key);
        return Ok(HttpResponse::BadRequest().body("No data in data list"));
    }

    info!("Serializing offset and size and uploading");

    let offset_size_bytes = serialize_offset_size(&offset_size_list)?;

    db.upload_sql(&key, &offset_size_bytes)
        .map_err(ErrorInternalServerError)?;

    info!("Data uploaded successfully with key: {}", key);
    Ok(HttpResponse::Ok().body(format!("Data uploaded successfully: key = {}", key)))
}

pub async fn get_service(key: String, req: HttpRequest)-> Result<HttpResponse, Error>{

    let user = header_handler(req)?;

    let db = Database::new(&user)?;
    db.check_key_nonexistance(&key)?;
    info!("Retrieving data for key: {}", key);

    // Connect to the SQLite database and retrieve offset and size data
    let offset_size_bytes = match db.get_offset_size_lists(&key) {
        Ok(offset_size_bytes) => offset_size_bytes,
        Err(e) => {
            warn!("Key does not exist or database error: {}", e);
            return Ok(HttpResponse::BadRequest().body("Key does not exist"));
        }
    };

    // Deserialize offset and size data
    let offset_size_list = deserialize_offset_size(&offset_size_bytes)?;

    let data = get_files_from_storage(&user,offset_size_list)?;

    // Return the FlatBuffers serialized data
    Ok(HttpResponse::Ok()
        .content_type("application/octet-stream")
        .body(data))
}

pub async fn append_service(key: String, mut payload: web::Payload, req: HttpRequest ) -> Result<HttpResponse, Error> {
    let user = header_handler(req)?;

    let db = Database::new(&user)?;
    db.check_key_nonexistance(&key)?;
    info!("Starting chunk load");
    let mut bytes = BytesMut::new();
    while let Some(chunk) = payload.next().await {
        let chunk = chunk.map_err(ErrorInternalServerError)?;
        bytes.extend_from_slice(&chunk);
    }
    if bytes.is_empty() {
        error!("No data uploaded with key: {}", key);
        return Ok(HttpResponse::BadRequest().body("No data was uploaded"));
    }
    
    info!("Total received data size: {} bytes", bytes.len());

    let mut offset_size_list_append = write_files_to_storage(&user,&bytes)?;


    if offset_size_list_append.is_empty() {
        error!("No data in data list with key: {}", key);
        return Ok(HttpResponse::BadRequest().body("No data in data list"));
    }
   
    info!("Serializing offset and size and uploading");

    let offset_size_bytes = match db.get_offset_size_lists(&key) {
        Ok(offset_size_bytes) => offset_size_bytes,
        Err(e) => {
            warn!("Key does not exist or database error: {}", e);
            return Ok(HttpResponse::BadRequest().body("Key does not exist"));
        }
    };
    
        // Deserialize offset and size data
    let mut offset_size_list = deserialize_offset_size(&offset_size_bytes)?;

    
    offset_size_list.append(&mut offset_size_list_append);  // Appending offset_list_append to offset_list

    let offset_size_bytes_append = serialize_offset_size(&offset_size_list)?;

    db.append_sql(&key, &offset_size_bytes_append)
            .map_err(ErrorInternalServerError)?;
    
    info!("Data apended successfully with key: {}", key);
    Ok(HttpResponse::Ok().body(format!("Data appended successfully: key = {}", key)))
    
}

pub async fn delete_service(key: String, req: HttpRequest)-> Result<HttpResponse, Error>{

    let user = header_handler(req)?;

    let db = Database::new(&user)?;
    db.check_key_nonexistance(&key)?;
    let offset_size_bytes = match db.get_offset_size_lists(&key) {
        Ok(offset_size_bytes) => offset_size_bytes,
        Err(e) => {
            warn!("Key does not exist or database error: {}", e);
            return Ok(HttpResponse::BadRequest().body("Key does not exist"));
        }
    };
    let offset_size_list = deserialize_offset_size(&offset_size_bytes)?;
    // Deserialize offset and size data
    
    delete_and_log(&user,&key, offset_size_list)?;

    match db.delete_from_db(&key) {
        Ok(()) => Ok(HttpResponse::Ok().body(format!("File deleted successfully: key = {}", key))),
        Err(e) => Ok(HttpResponse::InternalServerError().body(format!("Failed to delete key: {}", e))),
    }
}

pub async fn update_key_service(old_key: String, new_key: String, req: HttpRequest)->  Result<HttpResponse, Error>{
    
    let user = header_handler(req)?;

    let db = Database::new(&user)?;
    db.check_key_nonexistance(&old_key)?;
    // Update the key in the database
    match db.update_key_from_db(&old_key, &new_key) {
        Ok(()) => Ok(HttpResponse::Ok().body(format!("Key updated successfully from {} to {}", old_key, new_key))),
        Err(e) => Ok(HttpResponse::InternalServerError().body(format!("Failed to update key: {}", e))),
    }
}

pub async  fn update_service(key: String, mut payload: web::Payload, req: HttpRequest ) ->  Result<HttpResponse, Error>{
    let user = header_handler(req)?;

    let db = Database::new(&user)?;
    db.check_key_nonexistance(&key)?;

    info!("Starting chunk load");
    let mut bytes = BytesMut::new();
    while let Some(chunk) = payload.next().await {
        let chunk = chunk.map_err(ErrorInternalServerError)?;
        bytes.extend_from_slice(&chunk);
    }

    if bytes.is_empty() {
        error!("No data uploaded with key: {}", key);
        return Ok(HttpResponse::BadRequest().body("No data was uploaded"));
    }
    
    info!("Total received data size: {} bytes", bytes.len());
    info!("Starting deserialization");
    
    let offset_size_list = write_files_to_storage(&user,&bytes)?;
   
    if offset_size_list.is_empty()  {
        error!("No data in data list with key: {}", key);
        return Ok(HttpResponse::BadRequest().body("No data in data list"));
    }
    

    let offset_size_bytes = serialize_offset_size(&offset_size_list)?;
    db.update_file_db(&key, &offset_size_bytes)
    .map_err(ErrorInternalServerError)?;

    info!("Data uploaded successfully with key: {}", key);
    Ok(HttpResponse::Ok().body(format!("Data uploaded successfully: key = {}", key)))


}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::test;
    use std::env;
    use tempfile::TempDir;

    /// Test utilities for creating mock HTTP requests and managing test environments
    struct TestSetup {
        _temp_dir: TempDir,
        _storage_dir: TempDir,
        user: String,
    }

    impl TestSetup {
        /// Creates a new test environment with isolated database and storage
        /// This ensures each test runs independently without affecting others
        fn new() -> Self {
            let temp_dir = TempDir::new().expect("Failed to create temp directory");
            let storage_dir = TempDir::new().expect("Failed to create storage temp directory");
            
            // Set environment variables for test isolation
            env::set_var("DB_FILE", temp_dir.path().join("test_metadata.sqlite"));
            env::set_var("STORAGE_DIRECTORY", storage_dir.path());
            
            let user = "test_user".to_string();
            
            TestSetup {
                _temp_dir: temp_dir,
                _storage_dir: storage_dir,
                user,
            }
        }

        /// Creates a mock HTTP request with required User header
        /// Real-world usage requires authentication headers for user isolation
        fn create_request_with_user(&self) -> HttpRequest {
            test::TestRequest::default()
                .insert_header(("User", self.user.as_str()))
                .to_http_request()
        }
    }

    #[actix_web::test]
    async fn test_header_handler_success() {
        // Test successful header extraction - fundamental requirement
        // Every service call requires proper user identification
        let setup = TestSetup::new();
        let req = setup.create_request_with_user();
        
        let result = header_handler(req);
        assert!(result.is_ok(), "Header handler should succeed with valid User header");
        let user = result.unwrap();
        assert_eq!(user, setup.user);
    }

    #[actix_web::test]
    async fn test_header_handler_missing_user() {
        // Test missing User header - security validation
        // System must reject requests without proper authentication
        let req = test::TestRequest::default().to_http_request(); // No User header
        
        let result = header_handler(req);
        assert!(result.is_err(), "Header handler should fail with missing User header");
    }

    #[actix_web::test]
    async fn test_database_creation() {
        // Test database creation and basic operations
        // Validates the core database functionality works in test environment
        let setup = TestSetup::new();
        
        // Test database creation succeeds
        let db_result = Database::new(&setup.user);
        assert!(db_result.is_ok(), "Database creation should succeed");
        
        let db = db_result.unwrap();
        
        // Test key checking for non-existent key
        let key_exists = db.check_key("nonexistent_key");
        assert!(key_exists.is_ok(), "Key check should succeed");
        assert!(!key_exists.unwrap(), "Non-existent key should return false");
    }

    #[actix_web::test]
    async fn test_storage_operations() {
        // Test storage operations work in test environment
        // Validates file writing and reading functionality
        let setup = TestSetup::new();
        let test_data = b"Test data for storage operations";
        
        // Test writing to storage
        let write_result = write_files_to_storage(&setup.user, test_data);
        assert!(write_result.is_ok(), "Storage write should succeed");
        
        let offset_size_list = write_result.unwrap();
        assert!(!offset_size_list.is_empty(), "Should have offset/size entries");
        
        // Test reading from storage
        let read_result = get_files_from_storage(&setup.user, offset_size_list);
        assert!(read_result.is_ok(), "Storage read should succeed");
        
        let retrieved_data = read_result.unwrap();
        assert!(!retrieved_data.is_empty(), "Retrieved data should not be empty");
    }

    #[actix_web::test]
    async fn test_serialization_operations() {
        // Test serialization and deserialization operations
        // Validates the FlatBuffers serialization functionality
        let test_offsets = vec![(0u64, 100u64), (100u64, 200u64), (300u64, 150u64)];
        
        // Test serialization
        let serialize_result = serialize_offset_size(&test_offsets);
        assert!(serialize_result.is_ok(), "Serialization should succeed");
        
        let serialized_data = serialize_result.unwrap();
        assert!(!serialized_data.is_empty(), "Serialized data should not be empty");
        
        // Test deserialization
        let deserialize_result = deserialize_offset_size(&serialized_data);
        assert!(deserialize_result.is_ok(), "Deserialization should succeed");
        
        let deserialized_offsets = deserialize_result.unwrap();
        assert_eq!(deserialized_offsets.len(), test_offsets.len(), "Should have same number of entries");
        
        // Verify data integrity
        for (i, (original_offset, original_size)) in test_offsets.iter().enumerate() {
            let (deserialized_offset, deserialized_size) = deserialized_offsets[i];
            assert_eq!(*original_offset, deserialized_offset, "Offsets should match");
            assert_eq!(*original_size, deserialized_size, "Sizes should match");
        }
    }

    #[actix_web::test]
    async fn test_database_key_operations() {
        // Test comprehensive database key operations
        // Validates CRUD operations on database keys
        let setup = TestSetup::new();
        let db = Database::new(&setup.user).unwrap();
        let test_key = "test_database_key";
        let test_data = vec![1, 2, 3, 4, 5];
        
        // Test key doesn't exist initially
        assert!(!db.check_key(test_key).unwrap(), "Key should not exist initially");
        
        // Test upload operation
        let upload_result = db.upload_sql(test_key, &test_data);
        assert!(upload_result.is_ok(), "Upload should succeed");
        
        // Test key now exists
        assert!(db.check_key(test_key).unwrap(), "Key should exist after upload");
        
        // Test retrieval operation
        let retrieve_result = db.get_offset_size_lists(test_key);
        assert!(retrieve_result.is_ok(), "Retrieval should succeed");
        
        let retrieved_data = retrieve_result.unwrap();
        assert_eq!(retrieved_data, test_data, "Retrieved data should match uploaded data");
        
        // Test update operation
        let updated_data = vec![6, 7, 8, 9, 10];
        let update_result = db.update_file_db(test_key, &updated_data);
        assert!(update_result.is_ok(), "Update should succeed");
        
        // Verify update worked
        let updated_retrieved = db.get_offset_size_lists(test_key).unwrap();
        assert_eq!(updated_retrieved, updated_data, "Updated data should match");
        
        // Test key renaming
        let new_key = "renamed_test_key";
        let rename_result = db.update_key_from_db(test_key, new_key);
        assert!(rename_result.is_ok(), "Key rename should succeed");
        
        // Verify old key is gone and new key exists
        assert!(!db.check_key(test_key).unwrap(), "Old key should not exist");
        assert!(db.check_key(new_key).unwrap(), "New key should exist");
        
        // Test deletion
        let delete_result = db.delete_from_db(new_key);
        assert!(delete_result.is_ok(), "Deletion should succeed");
        
        // Verify key is gone
        assert!(!db.check_key(new_key).unwrap(), "Key should not exist after deletion");
    }

    #[actix_web::test]
    async fn test_full_data_workflow() {
        // Test complete data workflow - real-world usage pattern
        // This demonstrates the typical data lifecycle through all components
        let setup = TestSetup::new();
        let db = Database::new(&setup.user).unwrap();
        
        // Phase 1: Upload data
        let test_data = b"Complete workflow test data with sufficient length for chunking";
        let key = "workflow_test_key";
        
        // Write data to storage
        let offset_size_list = write_files_to_storage(&setup.user, test_data).unwrap();
        assert!(!offset_size_list.is_empty(), "Should have storage entries");
        
        // Serialize offset/size information
        let serialized_metadata = serialize_offset_size(&offset_size_list).unwrap();
        
        // Store metadata in database
        db.upload_sql(key, &serialized_metadata).unwrap();
        
        // Phase 2: Retrieve and verify data
        let retrieved_metadata = db.get_offset_size_lists(key).unwrap();
        assert_eq!(retrieved_metadata, serialized_metadata, "Metadata should match");
        
        let deserialized_offsets = deserialize_offset_size(&retrieved_metadata).unwrap();
        let retrieved_data = get_files_from_storage(&setup.user, deserialized_offsets.clone()).unwrap();
        
        // Note: Retrieved data is FlatBuffers serialized, so we verify it's not empty
        // and has reasonable size (original data gets wrapped in FlatBuffers format)
        assert!(!retrieved_data.is_empty(), "Retrieved data should not be empty");
        assert!(retrieved_data.len() >= test_data.len(), "Retrieved data should contain original data");
        
        // Phase 3: Append more data
        let append_data = b" + Additional appended data for testing";
        let append_offset_list = write_files_to_storage(&setup.user, append_data).unwrap();
        
        // Combine with existing metadata
        let mut combined_offsets = deserialized_offsets.clone();
        combined_offsets.extend(append_offset_list);
        
        let combined_metadata = serialize_offset_size(&combined_offsets).unwrap();
        db.append_sql(key, &combined_metadata).unwrap();
        
        // Verify appended data
        let final_metadata = db.get_offset_size_lists(key).unwrap();
        let final_offsets = deserialize_offset_size(&final_metadata).unwrap();
        assert!(final_offsets.len() > deserialized_offsets.len(), "Should have more entries after append");
        
        // Phase 4: Update with completely new data
        let update_data = b"Completely new data replacing the old content";
        let update_offset_list = write_files_to_storage(&setup.user, update_data).unwrap();
        let update_metadata = serialize_offset_size(&update_offset_list).unwrap();
        
        db.update_file_db(key, &update_metadata).unwrap();
        
        // Verify update
        let updated_metadata = db.get_offset_size_lists(key).unwrap();
        assert_eq!(updated_metadata, update_metadata, "Updated metadata should match");
        
        // Phase 5: Rename the key
        let new_key = "renamed_workflow_key";
        db.update_key_from_db(key, new_key).unwrap();
        
        // Verify rename
        assert!(!db.check_key(key).unwrap(), "Old key should not exist");
        assert!(db.check_key(new_key).unwrap(), "New key should exist");
        
        // Phase 6: Clean up
        let cleanup_metadata = db.get_offset_size_lists(new_key).unwrap();
        let cleanup_offsets = deserialize_offset_size(&cleanup_metadata).unwrap();
        
        // Delete storage files
        delete_and_log(&setup.user, new_key, cleanup_offsets).unwrap();
        
        // Delete database entry
        db.delete_from_db(new_key).unwrap();
        
        // Verify cleanup
        assert!(!db.check_key(new_key).unwrap(), "Key should not exist after cleanup");
    }

    #[actix_web::test]
    async fn test_error_handling() {
        // Test error handling scenarios
        // Validates system behavior under error conditions
        let setup = TestSetup::new();
        let db = Database::new(&setup.user).unwrap();
        
        // Test operations on non-existent keys
        let nonexistent_key = "this_key_does_not_exist";
        
        let get_result = db.get_offset_size_lists(nonexistent_key);
        assert!(get_result.is_err(), "Get on non-existent key should fail");
        
        // Test check_key_nonexistence function
        let check_result = db.check_key_nonexistance(nonexistent_key);
        assert!(check_result.is_err(), "Check non-existence should fail for missing key");
        
        // Test with existing key
        let existing_key = "existing_key";
        let test_data = vec![1, 2, 3];
        db.upload_sql(existing_key, &test_data).unwrap();
        
        let existence_check = db.check_key_nonexistance(existing_key);
        assert!(existence_check.is_err(), "Check non-existence should fail for existing key");
    }

    #[actix_web::test] 
    async fn test_multi_user_isolation() {
        // Test multi-user data isolation
        // Validates that different users cannot access each other's data
        let temp_dir = TempDir::new().unwrap();
        let storage_dir = TempDir::new().unwrap();
        
        env::set_var("DB_FILE", temp_dir.path().join("multi_user_test.sqlite"));
        env::set_var("STORAGE_DIRECTORY", storage_dir.path());
        
        let user1 = "user1";
        let user2 = "user2";
        let test_key = "shared_key_name";
        let user1_data = vec![1, 2, 3];
        let user2_data = vec![4, 5, 6];
        
        // Upload data for both users with same key name
        let db1 = Database::new(user1).unwrap();
        let db2 = Database::new(user2).unwrap();
        
        db1.upload_sql(test_key, &user1_data).unwrap();
        db2.upload_sql(test_key, &user2_data).unwrap();
        
        // Verify each user can only access their own data
        let user1_retrieved = db1.get_offset_size_lists(test_key).unwrap();
        let user2_retrieved = db2.get_offset_size_lists(test_key).unwrap();
        
        assert_eq!(user1_retrieved, user1_data, "User1 should get their own data");
        assert_eq!(user2_retrieved, user2_data, "User2 should get their own data");
        assert_ne!(user1_retrieved, user2_retrieved, "Users should have different data");
        
        // Verify key existence is user-specific
        assert!(db1.check_key(test_key).unwrap(), "User1 should see their key");
        assert!(db2.check_key(test_key).unwrap(), "User2 should see their key");
        
        // Delete user1's key
        db1.delete_from_db(test_key).unwrap();
        
        // Verify user1's key is gone but user2's remains
        assert!(!db1.check_key(test_key).unwrap(), "User1's key should be deleted");
        assert!(db2.check_key(test_key).unwrap(), "User2's key should still exist");
    }
}

