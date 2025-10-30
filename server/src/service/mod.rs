//service/mod.rs
pub mod metadata_service;
pub mod user_context;
pub mod storage_service;

use actix_web::{ web, HttpResponse,Error, HttpRequest};
use futures::StreamExt;
use bytes::BytesMut;
use log::{info, error, warn};
use actix_web::error::{ErrorInternalServerError,ErrorBadRequest};
use log_mdc;


use crate::service::storage_service::{StorageService, StorageMode};
use crate::service::metadata_service::MetadataService;
use crate::service::user_context::UserContext;
use crate::util::serializer::{serialize_offset_size, deserialize_offset_size};


fn header_handler(req: HttpRequest) -> Result<UserContext, Error> {
    let user_id = req.headers()
        .get("User")
        .ok_or_else(|| ErrorBadRequest("Missing User header"))?
        .to_str()
        .map_err(|_| ErrorBadRequest("Invalid User header value"))?
        .to_string();
    
    // Extract bucket from header, default to "default"
    let bucket = req.headers()
        .get("Bucket")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("default")
        .to_string();
    
    log_mdc::insert("user", &user_id);
    log_mdc::insert("bucket", &bucket);
    
    let mut context = UserContext::with_bucket(user_id, bucket);
    
    // Extract any additional headers as metadata
    for (header_name, header_value) in req.headers() {
        if let Ok(value_str) = header_value.to_str() {
            if header_name.as_str() != "user" && header_name.as_str() != "bucket" {
                context.set_metadata(header_name.as_str().to_string(), value_str.to_string());
            }
        }
    }
    
    Ok(context)
}

pub async fn put_service(key: String, mut payload: web::Payload, req: HttpRequest) -> Result<HttpResponse, Error>{

    let context = header_handler(req)?;
    info!("PUT service called for user: {}, bucket: {}, key: {}", context.user_id, context.bucket, key);

    let db = MetadataService::new(&context.user_id)?;
    info!("MetadataService created for user: {}", context.user_id);
    
    let key_exists = db.check_key(&context.bucket, &key).map_err(ErrorInternalServerError)?;
    info!("Key exists check result: {} for key: {} in bucket: {}", key_exists, key, context.bucket);
    
    if key_exists {
        warn!("Key already exists: {} in bucket: {}", key, context.bucket);
        return Ok(HttpResponse::BadRequest().body("Key already exists"));
    }

    info!("Starting chunk load for user: {}, bucket: {}", context.user_id, context.bucket);
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

    // Write incoming FlatBuffers payload to storage and collect (offset, size)
    let storage_service = StorageService::new();
    let offset_size_list = storage_service.write_object(&context, &bytes, StorageMode::Native)?;


    if offset_size_list.is_empty()  {
        error!("No data in data list with key: {}", key);
        return Ok(HttpResponse::BadRequest().body("No data in data list"));
    }

    info!("Serializing offset and size and uploading");

    info!("Serializing offset_size_list with {} entries", offset_size_list.len());
    let offset_size_bytes = serialize_offset_size(&offset_size_list)?;
    info!("Successfully serialized offset_size_bytes, size: {} bytes", offset_size_bytes.len());

    info!("Writing metadata for user: {}, bucket: {}, key: {}", context.user_id, context.bucket, key);
    db.write_metadata(&context.bucket, &key, &offset_size_bytes)
        .map_err(|e| {
            error!("Failed to write metadata for user: {}, bucket: {}, key: {}: {}", context.user_id, context.bucket, key, e);
            ErrorInternalServerError(e)
        })?;
    info!("Successfully wrote metadata for user: {}, bucket: {}, key: {}", context.user_id, context.bucket, key);

    info!("Data uploaded successfully with key: {} in bucket: {}", key, context.bucket);
    Ok(HttpResponse::Ok().body(format!("Data uploaded successfully: key = {}, bucket = {}", key, context.bucket)))
}

pub async fn get_service(key: String, req: HttpRequest)-> Result<HttpResponse, Error>{

    let context = header_handler(req)?;

    let db = MetadataService::new(&context.user_id)?;
    db.check_key_nonexistance(&context.bucket, &key)?;
    info!("Retrieving data for key: {} in bucket: {}", key, context.bucket);

    // Connect to the SQLite database and retrieve offset and size data
    let offset_size_bytes = match db.read_metadata(&context.bucket, &key) {
        Ok(offset_size_bytes) => offset_size_bytes,
        Err(e) => {
            warn!("Key does not exist or database error: {}", e);
            return Ok(HttpResponse::BadRequest().body("Key does not exist"));
        }
    };

    // Deserialize offset and size data
    let offset_size_list = deserialize_offset_size(&offset_size_bytes)?;

    // Build FlatBuffers payload from stored chunks
    let storage_service = StorageService::new();
    let data = storage_service.read_object(&context, &offset_size_list, StorageMode::Native)?;

    // Return the FlatBuffers serialized data
    Ok(HttpResponse::Ok()
        .content_type("application/octet-stream")
        .body(data))
}

pub async fn append_service(key: String, mut payload: web::Payload, req: HttpRequest ) -> Result<HttpResponse, Error> {
    let context = header_handler(req)?;

    let db = MetadataService::new(&context.user_id)?;
    db.check_key_nonexistance(&context.bucket, &key)?;
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

    // Write additional FlatBuffers payload chunks to storage
    let storage_service = StorageService::new();
    let mut offset_size_list_append = storage_service.write_object(&context, &bytes, StorageMode::Native)?;


    if offset_size_list_append.is_empty() {
        error!("No data in data list with key: {}", key);
        return Ok(HttpResponse::BadRequest().body("No data in data list"));
    }
   
    info!("Serializing offset and size and uploading");

    let offset_size_bytes = match db.read_metadata(&context.bucket, &key) {
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

    db.append_metadata(&context.bucket, &key, &offset_size_bytes_append)
            .map_err(ErrorInternalServerError)?;
    
    info!("Data apended successfully with key: {}", key);
    Ok(HttpResponse::Ok().body(format!("Data appended successfully: key = {}", key)))
    
}

pub async fn delete_service(key: String, req: HttpRequest)-> Result<HttpResponse, Error>{

    let context = header_handler(req)?;
    let storage_service = StorageService::new();
    storage_service.delete_object(&context, &key)?;
    Ok(HttpResponse::Ok().body(format!("File deleted successfully: key = {} in bucket = {}", key, context.bucket)))
}

pub async fn update_key_service(old_key: String, new_key: String, req: HttpRequest)->  Result<HttpResponse, Error>{
    
    let context = header_handler(req)?;

    let db = MetadataService::new(&context.user_id)?;
    db.check_key_nonexistance(&context.bucket, &old_key)?;
    // Update the key in the database
    match db.rename_key(&context.bucket, &old_key, &new_key) {
        Ok(()) => Ok(HttpResponse::Ok().body(format!("Key updated successfully from {} to {} in bucket {}", old_key, new_key, context.bucket))),
        Err(e) => Ok(HttpResponse::InternalServerError().body(format!("Failed to update key: {}", e))),
    }
}

pub async  fn update_service(key: String, mut payload: web::Payload, req: HttpRequest ) ->  Result<HttpResponse, Error>{
    let context = header_handler(req)?;

    let db = MetadataService::new(&context.user_id)?;
    db.check_key_nonexistance(&context.bucket, &key)?;

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
    
    // Rewrite with provided FlatBuffers payload
    let storage_service = StorageService::new();
    let offset_size_list = storage_service.write_object(&context, &bytes, StorageMode::Native)?;
   
    if offset_size_list.is_empty()  {
        error!("No data in data list with key: {}", key);
        return Ok(HttpResponse::BadRequest().body("No data in data list"));
    }
    

    let offset_size_bytes = serialize_offset_size(&offset_size_list)?;
    db.update_metadata(&context.bucket, &key, &offset_size_bytes)
    .map_err(ErrorInternalServerError)?;

    info!("Data uploaded successfully with key: {} in bucket: {}", key, context.bucket);
    Ok(HttpResponse::Ok().body(format!("Data uploaded successfully: key = {}, bucket = {}", key, context.bucket)))


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
        let context = result.unwrap();
        assert_eq!(context.user_id, "test_user");
        assert_eq!(context.bucket, "default");
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
        let context = result.unwrap();
        assert_eq!(context.user_id, "");
        assert_eq!(context.bucket, "default");
        println!("Header handler with empty user test passed!");
    }
}