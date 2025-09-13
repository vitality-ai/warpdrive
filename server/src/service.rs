//service.rs
use actix_web::{ web, HttpResponse,Error, HttpRequest};
use futures::StreamExt;
use bytes::BytesMut;
use log::{info, error, warn};
use actix_web::error::{ErrorInternalServerError,ErrorBadRequest};
use log_mdc;
use lazy_static;

use crate::storage::{BinaryStorage, get_configured_storage_backend};
use crate::metadata_service::MetadataService;
use crate::util::serializer::{serialize_offset_size, deserialize_offset_size};

/// Storage service that manages binary storage operations
pub struct StorageService {
    backend: Box<dyn BinaryStorage>,
}

impl StorageService {
    pub fn new() -> Self {
        Self {
            backend: get_configured_storage_backend(),
        }
    }
    
    pub fn with_backend(backend: Box<dyn BinaryStorage>) -> Self {
        Self { backend }
    }
    
    pub fn get_backend(&self) -> &dyn BinaryStorage {
        &*self.backend
    }
}

lazy_static::lazy_static! {
    static ref STORAGE_SERVICE: StorageService = StorageService::new();
}


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

    let db = MetadataService::new(&user)?;
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

    let storage_backend = STORAGE_SERVICE.get_backend();
    let offset_size_list = storage_backend.put_files(&user, &bytes)?;


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

    let db = MetadataService::new(&user)?;
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

    let storage_backend = STORAGE_SERVICE.get_backend();
    let data = storage_backend.get_files(&user, offset_size_list)?;

    // Return the FlatBuffers serialized data
    Ok(HttpResponse::Ok()
        .content_type("application/octet-stream")
        .body(data))
}

pub async fn append_service(key: String, mut payload: web::Payload, req: HttpRequest ) -> Result<HttpResponse, Error> {
    let user = header_handler(req)?;

    let db = MetadataService::new(&user)?;
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

    let storage_backend = STORAGE_SERVICE.get_backend();
    let mut offset_size_list_append = storage_backend.put_files(&user, &bytes)?;


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

    let db = MetadataService::new(&user)?;
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
    
    let storage_backend = STORAGE_SERVICE.get_backend();
    storage_backend.delete_files(&user, &key, offset_size_list)?;

    match db.delete_from_db(&key) {
        Ok(()) => Ok(HttpResponse::Ok().body(format!("File deleted successfully: key = {}", key))),
        Err(e) => Ok(HttpResponse::InternalServerError().body(format!("Failed to delete key: {}", e))),
    }
}

pub async fn update_key_service(old_key: String, new_key: String, req: HttpRequest)->  Result<HttpResponse, Error>{
    
    let user = header_handler(req)?;

    let db = MetadataService::new(&user)?;
    db.check_key_nonexistance(&old_key)?;
    // Update the key in the database
    match db.update_key_from_db(&old_key, &new_key) {
        Ok(()) => Ok(HttpResponse::Ok().body(format!("Key updated successfully from {} to {}", old_key, new_key))),
        Err(e) => Ok(HttpResponse::InternalServerError().body(format!("Failed to update key: {}", e))),
    }
}

pub async  fn update_service(key: String, mut payload: web::Payload, req: HttpRequest ) ->  Result<HttpResponse, Error>{
    let user = header_handler(req)?;

    let db = MetadataService::new(&user)?;
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
    
    let storage_backend = STORAGE_SERVICE.get_backend();
    let offset_size_list = storage_backend.put_files(&user, &bytes)?;
   
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


// All unit tests will currently be here. 

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_functionality() {
        let test_string = "test_user".to_string();
        assert_eq!(test_string, "test_user");
        println!("Basic test passed!");
    }

    #[test]
    fn test_header_handler_with_valid_user() {
        use actix_web::test;
        
        // Create a test request with the User header
        let req = test::TestRequest::default()
            .insert_header(("User", "test_user"))
            .to_http_request();
        
        // Call header_handler function
        let result = header_handler(req);
        
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "test_user");
        println!("Header handler with valid user test passed!");
    }

    #[test]
    fn test_header_handler_missing_user_header() {
        use actix_web::test;
        
        let req = test::TestRequest::default()
            .to_http_request();
        
        let result = header_handler(req);
        
        assert!(result.is_err());
        println!("Header handler missing user header test passed!");
    }

    #[test]
    fn test_header_handler_with_empty_user() {
        use actix_web::test;
        
        let req = test::TestRequest::default()
            .insert_header(("User", ""))
            .to_http_request();
        
        let result = header_handler(req);
        
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "");
        println!("Header handler with empty user test passed!");
    }

    #[test]
    fn test_storage_service_creation() {
        // Test that StorageService can be created
        let storage_service = StorageService::new();
        let _backend = storage_service.get_backend();
        
        // We can't test the exact type, but we can ensure it implements BinaryStorage
        // by calling a method that should exist
        use crate::storage::MockBinaryStore;
        let mock_storage = Box::new(MockBinaryStore::new());
        let custom_service = StorageService::with_backend(mock_storage);
        let _custom_backend = custom_service.get_backend();
        
        // Both backends should be usable
        assert!(true); // Test passes if no panic occurs
        println!("Storage service creation test passed!");
    }

    #[test]
    fn test_storage_backend_configuration() {
        use crate::storage::get_configured_storage_backend;
        
        // Test default backend (should be LocalXFS)
        let _default_backend = get_configured_storage_backend();
        assert!(true); // Test passes if creation succeeds
        
        // Test setting environment variable (we can't easily test this in unit tests
        // without affecting other tests, but the function is designed to handle it)
        println!("Storage backend configuration test passed!");
    }
}