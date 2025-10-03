// S3-compatible request handlers
use actix_web::{web, HttpRequest, HttpResponse, Error};
use log::{info, warn};
use futures::StreamExt;
use bytes::BytesMut;

use crate::s3::auth::{authenticate_s3_request, create_authenticated_request};
// use crate::service::{get_service, put_service, delete_service};
use crate::service::metadata_service::MetadataService;
use crate::service::user_context::UserContext;
// use crate::storage::{get_files_from_storage};
use crate::util::serializer::deserialize_offset_size;

/// S3-compatible PUT object handler
/// Handles requests like: PUT /s3/{bucket}/{key}
pub async fn s3_put_object_handler(
    path: web::Path<(String, String)>,
    mut payload: web::Payload,
    req: HttpRequest,
) -> Result<HttpResponse, Error> {
    // Check if this is a copy operation first
    if req.headers().contains_key("x-amz-copy-source") {
        return s3_copy_object_handler(path, req).await;
    }
    
    let (bucket, key) = path.into_inner();
    info!("S3 PUT Object: bucket={}, key={}", bucket, key);
    
    // Authenticate the request
    let auth_result = authenticate_s3_request(&req)?;
    
    // Log the authentication result
    info!("S3 Authentication successful: user={}, bucket={}", auth_result.user_id, auth_result.bucket);
    
    // Create authenticated request with proper headers
    let _authenticated_req = create_authenticated_request(&req, &auth_result);
    
    // Process the payload
    let mut bytes = BytesMut::new();
    while let Some(chunk) = payload.next().await {
        let chunk = chunk.map_err(|e| {
            warn!("Error reading payload chunk: {}", e);
            actix_web::error::ErrorInternalServerError("Error reading payload")
        })?;
        bytes.extend_from_slice(&chunk);
    }
    
    if bytes.is_empty() {
        warn!("Empty payload for PUT request");
        return Ok(HttpResponse::BadRequest().body("Empty payload"));
    }
    
    info!("S3 PUT: Processing {} bytes for bucket={}, key={}", bytes.len(), bucket, key);
    
    // Create user context for storage operations
    let context = UserContext::with_bucket(auth_result.user_id, auth_result.bucket);
    
    // Get metadata service
    let db = MetadataService::new(&context.user_id)?;
    
    // For S3 compatibility, allow overwriting existing objects
    // (S3 allows PUT to overwrite existing objects)
    
    // If object already exists, delete it first (S3 overwrite behavior)
    if db.check_key(&context.bucket, &key)? {
        info!("S3 PUT: Overwriting existing object for bucket={}, key={}", bucket, key);
        
        // Read existing metadata to get offset/size info for cleanup
        let existing_offset_size_bytes = db.read_metadata(&context.bucket, &key)?;
        let existing_offset_size_list = deserialize_offset_size(&existing_offset_size_bytes)?;
        
        // Delete existing data from storage
        crate::storage::delete_and_log(&context, &key, existing_offset_size_list)?;
        
        // Delete existing metadata
        db.delete_metadata(&context.bucket, &key)?;
    }
    
    // Use S3-compatible storage system for raw binary data
    let offset_size_list = crate::storage::write_s3_data_to_storage(&context, &bytes)?;
    
    if offset_size_list.is_empty() {
        return Ok(HttpResponse::BadRequest().body("No data to store"));
    }
    
    // Serialize and store metadata (same as put_service)
    let offset_size_bytes = crate::util::serializer::serialize_offset_size(&offset_size_list)?;
    db.write_metadata(&context.bucket, &key, &offset_size_bytes)?;
    
    // Return success response
    Ok(HttpResponse::Ok()
        .insert_header(("ETag", format!("\"{}\"", "s3-etag-placeholder")))
        .insert_header(("Content-Length", "0"))
        .body(""))
}

/// S3-compatible GET object handler
/// Handles requests like: GET /s3/{bucket}/{key}
pub async fn s3_get_object_handler(
    path: web::Path<(String, String)>,
    req: HttpRequest,
) -> Result<HttpResponse, Error> {
    let (bucket, key) = path.into_inner();
    info!("S3 GET Object: bucket={}, key={}", bucket, key);
    
    // Authenticate the request
    let auth_result = authenticate_s3_request(&req)?;
    
    // Log the authentication result
    info!("S3 Authentication successful: user={}, bucket={}", auth_result.user_id, auth_result.bucket);
    
    // Create authenticated request with proper headers
    let _authenticated_req = create_authenticated_request(&req, &auth_result);
    
    info!("S3 GET: Retrieving object for bucket={}, key={}", bucket, key);
    
    // Create user context for storage operations
    let context = UserContext::with_bucket(auth_result.user_id, auth_result.bucket);
    
    // Get metadata service
    let db = MetadataService::new(&context.user_id)?;
    
    // Check if key exists
    if !db.check_key(&context.bucket, &key)? {
        return Ok(HttpResponse::NotFound().body("Object not found"));
    }
    
    // Use S3-compatible storage system for raw binary data
    let offset_size_bytes = db.read_metadata(&context.bucket, &key)?;
    let offset_size_list = deserialize_offset_size(&offset_size_bytes)?;
    let data = crate::storage::get_s3_data_from_storage(&context, offset_size_list)?;
    
    // Return the actual data
    Ok(HttpResponse::Ok()
        .content_type("application/octet-stream")
        .body(data))
}

/// S3-compatible DELETE object handler
/// Handles requests like: DELETE /s3/{bucket}/{key}
pub async fn s3_delete_object_handler(
    path: web::Path<(String, String)>,
    req: HttpRequest,
) -> Result<HttpResponse, Error> {
    let (bucket, key) = path.into_inner();
    info!("S3 DELETE Object: bucket={}, key={}", bucket, key);
    
    // Authenticate the request
    let auth_result = authenticate_s3_request(&req)?;
    
    // Log the authentication result
    info!("S3 Authentication successful: user={}, bucket={}", auth_result.user_id, auth_result.bucket);
    
    // Create authenticated request with proper headers
    let _authenticated_req = create_authenticated_request(&req, &auth_result);
    
    info!("S3 DELETE: Deleting object for bucket={}, key={}", bucket, key);
    
    // Create user context for storage operations
    let context = UserContext::with_bucket(auth_result.user_id, auth_result.bucket);
    
    // Get metadata service
    let db = MetadataService::new(&context.user_id)?;
    
    // Check if key exists
    if !db.check_key(&context.bucket, &key)? {
        return Ok(HttpResponse::NotFound().body("Object not found"));
    }
    
    // Use the existing storage system (same as delete_service)
    let offset_size_bytes = db.read_metadata(&context.bucket, &key)?;
    let offset_size_list = deserialize_offset_size(&offset_size_bytes)?;
    
    // Delete from storage
    crate::storage::delete_and_log(&context, &key, offset_size_list)?;
    
    // Delete metadata
    db.delete_metadata(&context.bucket, &key)?;
    
    // Return success response
    Ok(HttpResponse::Ok()
        .insert_header(("Content-Length", "0"))
        .body(""))
}

/// S3-compatible HEAD object handler (for metadata)
/// Handles requests like: HEAD /s3/{bucket}/{key}
pub async fn s3_head_object_handler(
    path: web::Path<(String, String)>,
    req: HttpRequest,
) -> Result<HttpResponse, Error> {
    let (bucket, key) = path.into_inner();
    info!("S3 HEAD Object: bucket={}, key={}", bucket, key);
    
    // Authenticate the request
    let auth_result = authenticate_s3_request(&req)?;
    
    // Log the authentication result
    info!("S3 Authentication successful: user={}, bucket={}", auth_result.user_id, auth_result.bucket);
    
    // For HEAD requests, we just return metadata without the body
    // This is a simplified implementation - in practice, you'd check if the object exists
    // and return appropriate headers
    
    Ok(HttpResponse::Ok()
        .insert_header(("Content-Type", "application/octet-stream"))
        .insert_header(("Content-Length", "0"))
        .body(""))
}

/// S3-compatible list objects handler
/// Handles requests like: GET /s3/{bucket}?list-type=2
pub async fn s3_list_objects_handler(
    path: web::Path<String>,
    query: web::Query<std::collections::HashMap<String, String>>,
    req: HttpRequest,
) -> Result<HttpResponse, Error> {
    let bucket = path.into_inner();
    info!("S3 List Objects: bucket={}", bucket);
    
    // Authenticate the request
    let auth_result = authenticate_s3_request(&req)?;
    
    // Log the authentication result
    info!("S3 Authentication successful: user={}, bucket={}", auth_result.user_id, auth_result.bucket);
    
    // Check if this is a list request
    if query.get("list-type") != Some(&"2".to_string()) {
        return Ok(HttpResponse::BadRequest().body("Invalid list request"));
    }
    
    // Return a simple S3 XML list response
    let xml_response = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
    <Name>{}</Name>
    <Prefix></Prefix>
    <KeyCount>0</KeyCount>
    <MaxKeys>1000</MaxKeys>
    <IsTruncated>false</IsTruncated>
</ListBucketResult>"#,
        bucket
    );
    
    Ok(HttpResponse::Ok()
        .content_type("application/xml")
        .body(xml_response))
}

/// S3-compatible COPY object handler
/// Handles requests like: PUT /s3/{bucket}/{key} with x-amz-copy-source header
pub async fn s3_copy_object_handler(
    path: web::Path<(String, String)>,
    req: HttpRequest,
) -> Result<HttpResponse, Error> {
    let (bucket, key) = path.into_inner();
    info!("S3 COPY Object: bucket={}, key={}", bucket, key);
    
    // Authenticate the request
    let auth_result = authenticate_s3_request(&req)?;
    
    // Log the authentication result
    info!("S3 Authentication successful: user={}, bucket={}", auth_result.user_id, auth_result.bucket);
    
    // Get copy source from headers
    let copy_source = match req.headers().get("x-amz-copy-source") {
        Some(header) => header.to_str().map_err(|_| actix_web::error::ErrorBadRequest("Invalid copy source header"))?,
        None => return Ok(HttpResponse::BadRequest().body("Missing x-amz-copy-source header")),
    };
    
    // Parse copy source (format: bucket/key)
    let parts: Vec<&str> = copy_source.splitn(2, '/').collect();
    if parts.len() != 2 {
        return Ok(HttpResponse::BadRequest().body("Invalid copy source format"));
    }
    let (source_bucket, source_key) = (parts[0], parts[1]);
    
    info!("S3 COPY: Copying from {}/{} to {}/{}", source_bucket, source_key, bucket, key);
    
    // Create user context for storage operations
    let context = UserContext::with_bucket(auth_result.user_id, auth_result.bucket);
    
    // Get metadata service
    let db = MetadataService::new(&context.user_id)?;
    
    // Check if source exists
    if !db.check_key(&context.bucket, source_key)? {
        return Ok(HttpResponse::NotFound().body("Source object not found"));
    }
    
    // For S3 compatibility, allow overwriting existing objects in copy operation
    // If destination already exists, delete it first
    if db.check_key(&context.bucket, &key)? {
        info!("S3 COPY: Overwriting existing destination object for bucket={}, key={}", bucket, key);
        
        // Read existing metadata to get offset/size info for cleanup
        let existing_offset_size_bytes = db.read_metadata(&context.bucket, &key)?;
        let existing_offset_size_list = deserialize_offset_size(&existing_offset_size_bytes)?;
        
        // Delete existing data from storage
        crate::storage::delete_and_log(&context, &key, existing_offset_size_list)?;
        
        // Delete existing metadata
        db.delete_metadata(&context.bucket, &key)?;
    }
    
    // Read source metadata
    let offset_size_bytes = db.read_metadata(&context.bucket, source_key)?;
    let offset_size_list = deserialize_offset_size(&offset_size_bytes)?;
    
    // Get source data using S3-compatible function
    let source_data = crate::storage::get_s3_data_from_storage(&context, offset_size_list)?;
    
    // Write to new location using S3-compatible function
    let new_offset_size_list = crate::storage::write_s3_data_to_storage(&context, &source_data)?;
    let new_offset_size_bytes = crate::util::serializer::serialize_offset_size(&new_offset_size_list)?;
    db.write_metadata(&context.bucket, &key, &new_offset_size_bytes)?;
    
    // Return success response with copy result XML
    let copy_result_xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<CopyObjectResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
    <ETag>"s3-etag-placeholder"</ETag>
    <LastModified>{}</LastModified>
</CopyObjectResult>"#,
        chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S.000Z")
    );
    
    Ok(HttpResponse::Ok()
        .content_type("application/xml")
        .body(copy_result_xml))
}
