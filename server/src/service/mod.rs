//service/mod.rs
pub mod metadata_service;
pub mod storage_service;
pub mod user_context;

use actix_web::{ web, HttpResponse,Error, HttpRequest};
use futures::StreamExt;
use bytes::BytesMut;
use log::{info, error, warn, debug};
use actix_web::error::{ErrorInternalServerError,ErrorBadRequest};
use log_mdc;


use crate::service::metadata_service::MetadataService;
use crate::service::storage_service::StorageService;
use crate::service::user_context::UserContext;
use crate::app_state::AppState;
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

pub async fn put_service(key: String, mut payload: web::Payload, req: HttpRequest, app_state: web::Data<AppState>) -> Result<HttpResponse, Error>{

    let context = header_handler(req)?;
    debug!("PUT service called for user: {}, bucket: {}, key: {}", context.user_id, context.bucket, key);

    let mdservice = &app_state.metadata_service;
    debug!("MetadataService created for user: {}", context.user_id);
    
    let key_exists = mdservice.check_key(&context, &key).map_err(ErrorInternalServerError)?;
    debug!("Key exists check result: {} for key: {} in bucket: {}", key_exists, key, context.bucket);
    
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

    let offset_size_list = app_state.storage_service.write_files_to_storage(&context, &bytes, false)?;


    if offset_size_list.is_empty()  {
        error!("No data in data list with key: {}", key);
        return Ok(HttpResponse::BadRequest().body("No data in data list"));
    }

    debug!("Serializing offset and size and uploading");

    debug!("Serializing offset_size_list with {} entries", offset_size_list.len());
    let offset_size_bytes = serialize_offset_size(&offset_size_list)?;
    debug!("Successfully serialized offset_size_bytes, size: {} bytes", offset_size_bytes.len());

    debug!("Writing metadata for user: {}, bucket: {}, key: {}", context.user_id, context.bucket, key);
    mdservice.write_metadata(&context, &key, &offset_size_bytes)
        .map_err(|e| {
            error!("Failed to write metadata for user: {}, bucket: {}, key: {}: {}", context.user_id, context.bucket, key, e);
            ErrorInternalServerError(e)
        })?;
    debug!("Successfully wrote metadata for user: {}, bucket: {}, key: {}", context.user_id, context.bucket, key);

    debug!("Data uploaded successfully with key: {} in bucket: {}", key, context.bucket);
    Ok(HttpResponse::Ok().body(format!("Data uploaded successfully: key = {}, bucket = {}", key, context.bucket)))
}

pub async fn get_service(key: String, req: HttpRequest, app_state: web::Data<AppState>) -> Result<HttpResponse, Error>{

    let context = header_handler(req)?;

    let mdservice = &app_state.metadata_service;
    mdservice.check_key_nonexistance(&context, &key)?;
    info!("Retrieving data for key: {} in bucket: {}", key, context.bucket);

    // Connect to the SQLite database and retrieve offset and size data
    let offset_size_bytes = mdservice.read_metadata(&context, &key)
        .map_err(|e| {
            warn!("Key does not exist or database error: {}", e);
            ErrorBadRequest("Key does not exist")
        })?;

    // Deserialize offset and size data
    let offset_size_list = deserialize_offset_size(&offset_size_bytes)?;

    let data = app_state.storage_service.get_files_from_storage(&context, offset_size_list, false)?;

    // Return the FlatBuffers serialized data
    Ok(HttpResponse::Ok()
        .content_type("application/octet-stream")
        .body(data))
}

pub async fn append_service(key: String, mut payload: web::Payload, req: HttpRequest, app_state: web::Data<AppState>) -> Result<HttpResponse, Error> {
    let context = header_handler(req)?;

    let mdservice = &app_state.metadata_service;
    mdservice.check_key_nonexistance(&context, &key)?;
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

    let mut offset_size_list_append = app_state.storage_service.write_files_to_storage(&context, &bytes, false)?;


    if offset_size_list_append.is_empty() {
        error!("No data in data list with key: {}", key);
        return Ok(HttpResponse::BadRequest().body("No data in data list"));
    }
   
    debug!("Serializing offset and size and uploading");

    let offset_size_bytes = mdservice.read_metadata(&context, &key)
        .map_err(|e| {
            warn!("Key does not exist or database error: {}", e);
            ErrorBadRequest("Key does not exist")
        })?;
    
        // Deserialize offset and size data
    let mut offset_size_list = deserialize_offset_size(&offset_size_bytes)?;

    
    offset_size_list.append(&mut offset_size_list_append);  // Appending offset_list_append to offset_list

    let offset_size_bytes_append = serialize_offset_size(&offset_size_list)?;

    mdservice.append_metadata(&context, &key, &offset_size_bytes_append)
            .map_err(ErrorInternalServerError)?;
    
    info!("Data apended successfully with key: {}", key);
    Ok(HttpResponse::Ok().body(format!("Data appended successfully: key = {}", key)))
    
}

pub async fn delete_service(key: String, req: HttpRequest, app_state: web::Data<AppState>) -> Result<HttpResponse, Error>{

    let context = header_handler(req)?;

    let mdservice = &app_state.metadata_service;
    mdservice.check_key_nonexistance(&context, &key)?;
    let offset_size_bytes = mdservice.read_metadata(&context, &key)
        .map_err(|e| {
            warn!("Key does not exist or database error: {}", e);
            ErrorBadRequest("Key does not exist")
        })?;
    let offset_size_list = deserialize_offset_size(&offset_size_bytes)?;
    // Deserialize offset and size data
    
    // Queue deletion for background processing and delete metadata immediately
    mdservice.queue_deletion(&context.user_id, &context.bucket, &key, &offset_size_list)?;
    
    Ok(HttpResponse::Ok().body(format!("File deleted successfully: key = {} in bucket = {}", key, context.bucket)))
}

pub async fn update_key_service(old_key: String, new_key: String, req: HttpRequest, app_state: web::Data<AppState>) -> Result<HttpResponse, Error>{
    
    let context = header_handler(req)?;

    let mdservice = &app_state.metadata_service;
    mdservice.check_key_nonexistance(&context, &old_key)?;
    // Update the key in the database
    match mdservice.rename_key(&context, &old_key, &new_key) {
        Ok(()) => Ok(HttpResponse::Ok().body(format!("Key updated successfully from {} to {} in bucket {}", old_key, new_key, context.bucket))),
        Err(e) => Ok(HttpResponse::InternalServerError().body(format!("Failed to update key: {}", e))),
    }
}

pub async  fn update_service(key: String, mut payload: web::Payload, req: HttpRequest, app_state: web::Data<AppState> ) ->  Result<HttpResponse, Error>{
    let context = header_handler(req)?;

    let mdservice = &app_state.metadata_service;
    mdservice.check_key_nonexistance(&context, &key)?;

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
    
    let offset_size_list = app_state.storage_service.write_files_to_storage(&context, &bytes, false)?;
   
    if offset_size_list.is_empty()  {
        error!("No data in data list with key: {}", key);
        return Ok(HttpResponse::BadRequest().body("No data in data list"));
    }
    

    let offset_size_bytes = serialize_offset_size(&offset_size_list)?;
    mdservice.update_metadata(&context, &key, &offset_size_bytes)
    .map_err(ErrorInternalServerError)?;

    debug!("Data uploaded successfully with key: {} in bucket: {}", key, context.bucket);
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