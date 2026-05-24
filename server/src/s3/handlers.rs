// S3-compatible request handlers
use actix_web::{web, HttpRequest, HttpResponse, Error};
use log::{debug, error, info, warn};
use bytes::{Bytes, BytesMut};
use futures::stream::{self, StreamExt as _};

use std::collections::HashMap;
use std::sync::Arc;

use crate::metadata::BucketStats;
use crate::s3::auth::{authenticate_s3_request, create_authenticated_request};
// use crate::service::{get_service, put_service, delete_service};
use crate::service::metadata_service::MetadataService;
use crate::service::user_context::UserContext;
use crate::service::storage_service::{StorageService, StorageMode};
use crate::storage::config::StorageConfig;
use crate::util::serializer::deserialize_offset_size;

/// S3 API XML namespace (ListAllMyBucketsResult, ListBucketResult, etc.)
const S3_XMLNS: &str = "http://s3.amazonaws.com/doc/2006-03-01/";
/// Optional Warpdrive extension elements on each `<Bucket>` for Console/dashboard (ignored by typical S3 clients).
const WARPDRIVE_LIST_BUCKETS_EXT_NS: &str = "http://warpdrive.vitality.dev/doc/listbuckets/1";

/// Max bytes read from disk per blocking slice when streaming S3 GetObject (bounds peak RAM).
const S3_GET_STREAM_CHUNK: u64 = 8 * 1024 * 1024;

/// Split metadata extents into <= `S3_GET_STREAM_CHUNK` pieces for streaming bodies.
fn s3_get_stream_slices(chunks: &[(u64, u64)]) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    for &(base, total) in chunks {
        let mut off = base;
        let mut rem = total;
        while rem > 0 {
            let n = rem.min(S3_GET_STREAM_CHUNK);
            out.push((off, n));
            off += n;
            rem -= n;
        }
    }
    out
}

fn xml_escape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

/// S3-compatible list buckets handler (AWS ListBuckets).
/// GET `/s3` or `/s3/` returns `ListAllMyBucketsResult` XML (`application/xml`).
/// Each bucket includes standard `Name` and `CreationDate`, plus optional extension elements
/// `ObjectCount` and `TotalSize` in `WARPDRIVE_LIST_BUCKETS_EXT_NS` for Vitality Console stats.
pub async fn s3_list_buckets_handler(req: HttpRequest) -> Result<HttpResponse, Error> {
    let auth_result = authenticate_s3_request(&req).await?;
    if !auth_result.bucket.is_empty() {
        return Ok(HttpResponse::BadRequest().body("Unexpected bucket in path for list-buckets"));
    }
    info!("S3 List Buckets: user={}", auth_result.user_id);
    let registered = auth_result.allowed_buckets.clone();
    let db = MetadataService::new(&auth_result.user_id)?;
    let haystack_stats = db.list_buckets_with_stats()?;
    let mut stats_by_name: HashMap<String, BucketStats> = haystack_stats
        .into_iter()
        .map(|s| (s.name.clone(), s))
        .collect();
    let mut stats: Vec<BucketStats> = Vec::new();
    for name in registered {
        if let Some(s) = stats_by_name.remove(&name) {
            stats.push(s);
        } else {
            stats.push(BucketStats {
                name,
                object_count: 0,
                total_size: 0,
            });
        }
    }
    stats.sort_by(|a, b| a.name.cmp(&b.name));
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S.000Z");
    let mut buckets_xml = String::new();
    for s in stats {
        buckets_xml.push_str(&format!(
            "    <Bucket>\n        <Name>{}</Name>\n        <CreationDate>{}</CreationDate>\n        <ObjectCount xmlns=\"{}\">{}</ObjectCount>\n        <TotalSize xmlns=\"{}\">{}</TotalSize>\n    </Bucket>\n",
            xml_escape_text(&s.name),
            now,
            WARPDRIVE_LIST_BUCKETS_EXT_NS,
            s.object_count,
            WARPDRIVE_LIST_BUCKETS_EXT_NS,
            s.total_size,
        ));
    }
    let owner_id = xml_escape_text(&auth_result.user_id);
    let xml_body = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<ListAllMyBucketsResult xmlns="{s3}">
    <Owner>
        <ID>{owner_id}</ID>
    </Owner>
    <Buckets>
{buckets_xml}    </Buckets>
</ListAllMyBucketsResult>"#,
        s3 = S3_XMLNS,
        owner_id = owner_id,
        buckets_xml = buckets_xml,
    );
    Ok(HttpResponse::Ok()
        .content_type("application/xml")
        .body(xml_body))
}

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
    
    // Check if this is a multipart upload part
    if let Ok(query) = web::Query::<std::collections::HashMap<String, String>>::from_query(req.query_string()) {
        if query.contains_key("partNumber") && query.contains_key("uploadId") {
            return s3_upload_part_handler(path, query, payload, req).await;
        }
    }
    
    let (bucket, key) = path.into_inner();
    info!("S3 PUT Object: bucket={}, key={}", bucket, key);
    
    // Authenticate the request
    let auth_result = authenticate_s3_request(&req).await?;
    
    // Log the authentication result
    debug!("S3 handler: authenticated user={}, bucket={}", auth_result.user_id, auth_result.bucket);
    
    // Create authenticated request with proper headers
    let _authenticated_req = create_authenticated_request(&req, &auth_result);

    // Context + metadata before consuming the body so overwrite can run first.
    let context = UserContext::with_bucket(auth_result.user_id, auth_result.bucket);
    let db = MetadataService::new(&context.user_id)?;

    // S3 overwrite: delete existing object before appending new extents.
    if db.check_key(&context.bucket, &key)? {
        info!("S3 PUT: Overwriting existing object for bucket={}, key={}", bucket, key);
        StorageService::new().delete_object(&context, &key)?;
    }

    // Stream body to storage: each HTTP chunk is appended as its own extent (bounded RAM ≈ chunk).
    let store = StorageConfig::from_env().create_store();
    let mut offset_size_list: Vec<(u64, u64)> = Vec::new();

    while let Some(chunk_result) = payload.next().await {
        let chunk = chunk_result.map_err(|e| {
            warn!("Error reading payload chunk: {}", e);
            actix_web::error::ErrorInternalServerError("Error reading payload")
        })?;
        if chunk.is_empty() {
            continue;
        }
        let context_c = context.clone();
        let store_c = Arc::clone(&store);
        let buf = chunk.to_vec();
        let pair = web::block(move || {
            store_c
                .write(&context_c.user_id, &context_c.bucket, &buf)
                .map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| {
            error!(
                "S3 PUT: blocking write task failed bucket={} key={} err={:?}",
                bucket, key, e
            );
            actix_web::error::ErrorInternalServerError("storage write task failed")
        })?;
        let (off, sz) = pair.map_err(|msg| {
            error!(
                "S3 PUT: write failed bucket={} key={} err={}",
                bucket, key, msg
            );
            actix_web::error::ErrorInternalServerError(msg)
        })?;
        offset_size_list.push((off, sz));
    }

    if offset_size_list.is_empty() {
        warn!("Empty payload for PUT request");
        return Ok(HttpResponse::BadRequest().body("Empty payload"));
    }

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
    let auth_result = authenticate_s3_request(&req).await?;
    
    // Log the authentication result
    debug!("S3 handler: authenticated user={}, bucket={}", auth_result.user_id, auth_result.bucket);
    
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
    let est_bytes: u64 = offset_size_list.iter().map(|(_, sz)| *sz as u64).sum();
    let stream_slices = Arc::new(s3_get_stream_slices(&offset_size_list));
    warn!(
        "S3 GET: streaming body bucket={} key={} est_bytes={} stream_slices={}",
        bucket,
        key,
        est_bytes,
        stream_slices.len()
    );

    // One store for the whole response: read_s3_extent() would call create_store() per slice.
    let store = StorageConfig::from_env().create_store();

    let ctx = context.clone();
    let bucket_log = bucket.clone();
    let key_log = key.clone();

    let byte_stream = stream::try_unfold(0usize, move |idx| {
        let slices = Arc::clone(&stream_slices);
        let context = ctx.clone();
        let bucket_log = bucket_log.clone();
        let key_log = key_log.clone();
        let store = Arc::clone(&store);
        async move {
            if idx >= slices.len() {
                return Ok::<Option<(Bytes, usize)>, Error>(None);
            }
            let (off, sz) = slices[idx];
            let context = context.clone();
            let chunk = web::block(move || {
                store
                    .read(&context.user_id, &context.bucket, off, sz)
                    .map_err(|e| e.to_string())
            })
            .await
            .map_err(|e| {
                error!(
                    "S3 GET stream: blocking task failed bucket={} key={} slice={} err={:?}",
                    bucket_log, key_log, idx, e
                );
                actix_web::error::ErrorInternalServerError(e)
            })?
            .map_err(|msg| {
                error!(
                    "S3 GET stream: read_extent failed bucket={} key={} slice={} err={}",
                    bucket_log, key_log, idx, msg
                );
                actix_web::error::ErrorInternalServerError(msg)
            })?;
            Ok(Some((Bytes::from(chunk), idx + 1)))
        }
    });

    Ok(HttpResponse::Ok()
        .content_type("application/octet-stream")
        .insert_header(("Content-Length", est_bytes.to_string()))
        .streaming(byte_stream))
}

/// S3-compatible DELETE object handler
/// Handles requests like: DELETE /s3/{bucket}/{key}
pub async fn s3_delete_object_handler(
    path: web::Path<(String, String)>,
    req: HttpRequest,
) -> Result<HttpResponse, Error> {
    // Check if this is an abort multipart upload
    if let Ok(query) = web::Query::<std::collections::HashMap<String, String>>::from_query(req.query_string()) {
        if query.contains_key("uploadId") {
            return s3_abort_multipart_upload_handler(path, query, req).await;
        }
    }
    
    let (bucket, key) = path.into_inner();
    info!("S3 DELETE Object: bucket={}, key={}", bucket, key);
    
    // Authenticate the request
    let auth_result = authenticate_s3_request(&req).await?;
    
    // Log the authentication result
    debug!("S3 handler: authenticated user={}, bucket={}", auth_result.user_id, auth_result.bucket);
    
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
    
    StorageService::new().delete_object(&context, &key)?;

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
    let auth_result = authenticate_s3_request(&req).await?;
    
    // Log the authentication result
    debug!("S3 handler: authenticated user={}, bucket={}", auth_result.user_id, auth_result.bucket);
    
    // For HEAD requests, we just return metadata without the body
    // This is a simplified implementation - in practice, you'd check if the object exists
    // and return appropriate headers
    
    Ok(HttpResponse::Ok()
        .insert_header(("Content-Type", "application/octet-stream"))
        .insert_header(("Content-Length", "0"))
        .body(""))
}

/// Validate S3 bucket name (3-63 chars, lowercase/numbers/hyphens/dots, no consecutive/leading/trailing hyphens or dots)
fn validate_bucket_name(bucket: &str) -> Result<(), Error> {
    if bucket.len() < 3 || bucket.len() > 63 {
        return Err(actix_web::error::ErrorBadRequest("Bucket name must be 3-63 characters"));
    }
    if bucket.starts_with('.') || bucket.ends_with('.') || bucket.starts_with('-') || bucket.ends_with('-') {
        return Err(actix_web::error::ErrorBadRequest("Bucket name cannot start or end with . or -"));
    }
    if bucket.contains("..") || bucket.contains("--") {
        return Err(actix_web::error::ErrorBadRequest("Bucket name cannot contain consecutive . or -"));
    }
    if !bucket.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-') {
        return Err(actix_web::error::ErrorBadRequest("Bucket name must be lowercase letters, numbers, hyphens, or dots only"));
    }
    Ok(())
}

/// S3-compatible create bucket handler (no-op: no bucket table in Warpdrive)
/// Handles PUT /s3/{bucket} - validates name and returns 201 for S3 API compatibility.
pub async fn s3_create_bucket_handler(
    path: web::Path<String>,
    req: HttpRequest,
) -> Result<HttpResponse, Error> {
    let bucket = path.into_inner();
    let _auth_result = authenticate_s3_request(&req).await?;
    validate_bucket_name(&bucket)?;
    info!("S3 Create Bucket (no-op): bucket={}", bucket);
    Ok(HttpResponse::Created()
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
    let auth_result = authenticate_s3_request(&req).await?;
    
    // Log the authentication result
    debug!("S3 handler: authenticated user={}, bucket={}", auth_result.user_id, auth_result.bucket);
    
    // Check if this is a list request
    if query.get("list-type") != Some(&"2".to_string()) {
        return Ok(HttpResponse::BadRequest().body("Invalid list request"));
    }
    
    // Create user context for storage operations
    let context = UserContext::with_bucket(auth_result.user_id, auth_result.bucket);
    
    // Get metadata service
    let db = MetadataService::new(&context.user_id)?;
    
    // List objects from metadata
    let objects = db.list_objects(&context.bucket)?;
    
    // Build XML response with actual objects
    let mut xml_objects = String::new();
    for object_key in &objects {
        xml_objects.push_str(&format!(
            "    <Contents>\n        <Key>{}</Key>\n        <Size>0</Size>\n        <LastModified>{}</LastModified>\n    </Contents>\n",
            object_key,
            chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S.000Z")
        ));
    }
    
    let xml_response = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
    <Name>{}</Name>
    <Prefix></Prefix>
    <KeyCount>{}</KeyCount>
    <MaxKeys>1000</MaxKeys>
    <IsTruncated>false</IsTruncated>
{}
</ListBucketResult>"#,
        bucket, objects.len(), xml_objects
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
    let auth_result = authenticate_s3_request(&req).await?;
    
    // Log the authentication result
    debug!("S3 handler: authenticated user={}, bucket={}", auth_result.user_id, auth_result.bucket);
    
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
        StorageService::new().delete_object(&context, &key)?;
    }
    
    // Read source metadata
    let offset_size_bytes = db.read_metadata(&context.bucket, source_key)?;
    let offset_size_list = deserialize_offset_size(&offset_size_bytes)?;
    
    // Read and combine source data
    let storage_service = StorageService::new();
    let source_data = storage_service.read_object(&context, &offset_size_list, StorageMode::S3)?;
    let new_offset_size_list = storage_service.write_object(&context, &source_data, StorageMode::S3)?;
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

/// S3-compatible create multipart upload handler
/// Handles requests like: POST /{bucket}/{key}?uploads
pub async fn s3_create_multipart_upload_handler(
    path: web::Path<(String, String)>,
    query: web::Query<std::collections::HashMap<String, String>>,
    req: HttpRequest,
) -> Result<HttpResponse, Error> {
    let (bucket, key) = path.into_inner();
    info!("S3 Create Multipart Upload: bucket={}, key={}", bucket, key);
    
    // Check if this is a multipart upload request
    if query.get("uploads") != Some(&"".to_string()) {
        return Ok(HttpResponse::BadRequest().body("Invalid multipart upload request"));
    }
    
    // Authenticate the request
    let _auth_result = authenticate_s3_request(&req).await?;
    
    // Generate upload ID
    let upload_id = format!("upload_{}", chrono::Utc::now().timestamp_millis());
    
    // Return XML response with upload ID
    let xml_response = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<InitiateMultipartUploadResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
    <Bucket>{}</Bucket>
    <Key>{}</Key>
    <UploadId>{}</UploadId>
</InitiateMultipartUploadResult>"#,
        bucket, key, upload_id
    );
    
    Ok(HttpResponse::Ok()
        .content_type("application/xml")
        .body(xml_response))
}

/// S3-compatible upload part handler
/// Handles requests like: PUT /{bucket}/{key}?partNumber=1&uploadId=...
pub async fn s3_upload_part_handler(
    path: web::Path<(String, String)>,
    query: web::Query<std::collections::HashMap<String, String>>,
    mut payload: web::Payload,
    req: HttpRequest,
) -> Result<HttpResponse, Error> {
    let (_bucket, key) = path.into_inner();
    
    // Get part number and upload ID from query
    let part_number = query.get("partNumber")
        .ok_or_else(|| actix_web::error::ErrorBadRequest("Missing partNumber"))?;
    let upload_id = query.get("uploadId")
        .ok_or_else(|| actix_web::error::ErrorBadRequest("Missing uploadId"))?;
    
    // Authenticate the request
    let auth_result = authenticate_s3_request(&req).await?;
    
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
        warn!("Empty payload for upload part");
        return Ok(HttpResponse::BadRequest().body("Empty payload"));
    }
    
    
    
    // For simplicity, we'll store the part data directly
    // In a real implementation, you'd store parts separately and combine them later
    let context = UserContext::with_bucket(auth_result.user_id, auth_result.bucket);
    let db = MetadataService::new(&context.user_id)?;
    
    // Store the part data as a single chunk
    let storage_service = StorageService::new();
    let offset_size_list = storage_service.write_object(&context, &bytes, StorageMode::S3)?;
    let offset_size_bytes = crate::util::serializer::serialize_offset_size(&offset_size_list)?;
    
    // Use a special key format for parts: {original_key}.part.{part_number}.{upload_id}
    let part_key = format!("{}.part.{}.{}", key, part_number, upload_id);
    db.write_metadata(&context.bucket, &part_key, &offset_size_bytes)?;
    
    // Return success response with ETag
    let etag = format!("\"{}\"", hex::encode(md5::compute(&bytes).0));
    Ok(HttpResponse::Ok()
        .insert_header(("ETag", etag))
        .body(""))
}

/// S3-compatible complete multipart upload handler
/// Handles requests like: POST /{bucket}/{key}?uploadId=...
pub async fn s3_complete_multipart_upload_handler(
    path: web::Path<(String, String)>,
    query: web::Query<std::collections::HashMap<String, String>>,
    mut payload: web::Payload,
    req: HttpRequest,
) -> Result<HttpResponse, Error> {
    let (bucket, key) = path.into_inner();
    
    // Get upload ID from query
    let upload_id = query.get("uploadId")
        .ok_or_else(|| actix_web::error::ErrorBadRequest("Missing uploadId"))?;
    
    // Authenticate the request
    let auth_result = authenticate_s3_request(&req).await?;
    
    // Process the payload to get part list
    let mut bytes = BytesMut::new();
    while let Some(chunk) = payload.next().await {
        let chunk = chunk.map_err(|e| {
            warn!("Error reading payload chunk: {}", e);
            actix_web::error::ErrorInternalServerError("Error reading payload")
        })?;
        bytes.extend_from_slice(&chunk);
    }
    
    // Parse the XML to get part numbers and ETags
    let _xml_content = String::from_utf8_lossy(&bytes);
    
    // For simplicity, we'll just combine all parts in order
    // In a real implementation, you'd parse the XML and combine parts in the correct order
    let context = UserContext::with_bucket(auth_result.user_id, auth_result.bucket);
    let db = MetadataService::new(&context.user_id)?;
    
    // Find all parts for this upload
    let all_objects = db.list_objects(&context.bucket)?;
    let mut parts: Vec<(i32, String)> = Vec::new();
    
    for obj_key in all_objects {
        if obj_key.starts_with(&format!("{}.part.", key)) && obj_key.ends_with(&format!(".{}", upload_id)) {
            // Extract part number from key format: {key}.part.{part_number}.{upload_id}
            let part_info = obj_key.strip_prefix(&format!("{}.part.", key))
                .and_then(|s| s.strip_suffix(&format!(".{}", upload_id)))
                .and_then(|s| s.parse::<i32>().ok());
            
            if let Some(part_number) = part_info {
                parts.push((part_number, obj_key));
            }
        }
    }
    
    // Sort parts by part number and build the final manifest directly from part manifests.
    // This avoids re-reading and rewriting the full object at completion time.
    parts.sort_by_key(|(part_num, _)| *part_num);
    if parts.is_empty() {
        return Ok(HttpResponse::BadRequest().body("No uploaded parts found for uploadId"));
    }
    let mut final_offset_size_list: Vec<(u64, u64)> = Vec::new();
    for (_part_num, part_key) in &parts {
        let offset_size_bytes = db.read_metadata(&context.bucket, part_key)?;
        let offset_size_list = deserialize_offset_size(&offset_size_bytes)?;
        final_offset_size_list.extend(offset_size_list);
    }
    
    
    // S3 overwrite semantics: if the final key already exists, remove the old
    // object first so metadata insert for the completed multipart object does
    // not fail on UNIQUE(user, bucket, key).
    if db.check_key(&context.bucket, &key)? {
        info!(
            "S3 CompleteMultipartUpload: overwriting existing object bucket={}, key={}",
            bucket, key
        );
        let storage_service = StorageService::new();
        storage_service.delete_object(&context, &key)?;
    }

    // Store final object metadata as an ordered list of already-uploaded part extents.
    let final_offset_size_bytes = crate::util::serializer::serialize_offset_size(&final_offset_size_list)?;
    db.write_metadata(&context.bucket, &key, &final_offset_size_bytes)?;
    
    // Clean up multipart part metadata. Keep chunk data: final object now references it.
    for (_, part_key) in &parts {
        db.delete_metadata(&context.bucket, part_key)?;
    }
    
    // Return success response
    let xml_response = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<CompleteMultipartUploadResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
    <Location>http://{}.s3.amazonaws.com/{}</Location>
    <Bucket>{}</Bucket>
    <Key>{}</Key>
    <ETag>"s3-etag-placeholder"</ETag>
</CompleteMultipartUploadResult>"#,
        bucket, key, bucket, key
    );
    
    Ok(HttpResponse::Ok()
        .content_type("application/xml")
        .body(xml_response))
}

/// S3-compatible abort multipart upload handler
/// Handles requests like: DELETE /{bucket}/{key}?uploadId=...
pub async fn s3_abort_multipart_upload_handler(
    path: web::Path<(String, String)>,
    query: web::Query<std::collections::HashMap<String, String>>,
    req: HttpRequest,
) -> Result<HttpResponse, Error> {
    let (bucket, key) = path.into_inner();
    info!("S3 Abort Multipart Upload: bucket={}, key={}", bucket, key);
    
    // Get upload ID from query
    let upload_id = query.get("uploadId")
        .ok_or_else(|| actix_web::error::ErrorBadRequest("Missing uploadId"))?;
    
    // Authenticate the request
    let auth_result = authenticate_s3_request(&req).await?;
    
    // Find and delete all parts for this upload
    let context = UserContext::with_bucket(auth_result.user_id, auth_result.bucket);
    let db = MetadataService::new(&context.user_id)?;
    
    let all_objects = db.list_objects(&context.bucket)?;
    for obj_key in all_objects {
        if obj_key.starts_with(&format!("{}.part.", key)) && obj_key.ends_with(&format!(".{}", upload_id)) {
            // Delete the part
            StorageService::new().delete_object(&context, &obj_key)?;
        }
    }
    
    Ok(HttpResponse::Ok()
        .insert_header(("Content-Length", "0"))
        .body(""))
}

/// S3 multipart router - routes POST requests to appropriate multipart handlers
/// based on query parameters
pub async fn s3_multipart_router(
    path: web::Path<(String, String)>,
    query: web::Query<std::collections::HashMap<String, String>>,
    payload: web::Payload,
    req: HttpRequest,
) -> Result<HttpResponse, Error> {
    let _bucket = path.0.clone();
    let _key = path.1.clone();
    
    // Route based on query parameters
    if query.contains_key("uploads") {
        // Create multipart upload
        s3_create_multipart_upload_handler(path, query, req).await
    } else if query.contains_key("uploadId") {
        // Complete multipart upload
        s3_complete_multipart_upload_handler(path, query, payload, req).await
    } else {
        // Unknown multipart operation
        Ok(HttpResponse::BadRequest().body("Invalid multipart operation"))
    }
}
