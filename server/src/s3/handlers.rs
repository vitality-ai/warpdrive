// S3-compatible request handlers
use actix_web::{web, HttpRequest, HttpResponse, Error, http::StatusCode};
use log::{debug, error, info, warn};
use bytes::Bytes;
use futures::stream::{self, StreamExt as _};

use std::collections::HashMap;
use std::sync::Arc;

use crate::metadata::Metadata;
use crate::s3::auth::{authenticate_s3_request, create_authenticated_request};
use crate::service::metadata_service::MetadataService;
use crate::service::user_context::UserContext;
use crate::service::storage_service::{StorageService, StorageMode};
use crate::storage::config::StorageConfig;
use crate::util::serializer::deserialize_offset_size;

const S3_XMLNS: &str = "http://s3.amazonaws.com/doc/2006-03-01/";
const WARPDRIVE_LIST_BUCKETS_EXT_NS: &str = "http://warpdrive.vitality.dev/doc/listbuckets/1";
const S3_GET_STREAM_CHUNK: u64 = 8 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&'  => out.push_str("&amp;"),
            '<'  => out.push_str("&lt;"),
            '>'  => out.push_str("&gt;"),
            '"'  => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c    => out.push(c),
        }
    }
    out
}

/// Build an S3-spec XML error response.
fn s3_error(status: StatusCode, code: &str, message: &str, resource: &str) -> HttpResponse {
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <Error>\n\
           <Code>{code}</Code>\n\
           <Message>{msg}</Message>\n\
           <Resource>{res}</Resource>\n\
           <RequestId>warpdrive</RequestId>\n\
         </Error>",
        code = xml_escape(code),
        msg  = xml_escape(message),
        res  = xml_escape(resource),
    );
    HttpResponse::build(status).content_type("application/xml").body(body)
}

/// Return 404 NoSuchBucket if the bucket is not registered for this user.
fn require_bucket(db: &MetadataService, bucket: &str) -> Result<(), HttpResponse> {
    match db.bucket_exists(bucket) {
        Ok(true)  => Ok(()),
        Ok(false) => Err(s3_error(StatusCode::NOT_FOUND, "NoSuchBucket",
                                  "The specified bucket does not exist", bucket)),
        Err(_)    => Err(s3_error(StatusCode::INTERNAL_SERVER_ERROR, "InternalError",
                                  "Internal server error", bucket)),
    }
}

/// Split extent list into ≤ S3_GET_STREAM_CHUNK slices for streaming.
fn stream_slices(chunks: &[(u64, u64)]) -> Vec<(u64, u64)> {
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

/// Compute RFC 2616 date string from now.
fn rfc2616_now() -> String {
    chrono::Utc::now().format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

/// Compute MD5 ETag (double-quoted) from accumulated bytes.
fn md5_etag(data: &[u8]) -> String {
    format!("\"{}\"", hex::encode(md5::compute(data).0))
}

/// Validate S3 bucket name rules.
fn validate_bucket_name(bucket: &str) -> Result<(), HttpResponse> {
    if bucket.len() < 3 || bucket.len() > 63 {
        return Err(s3_error(StatusCode::BAD_REQUEST, "InvalidBucketName",
                            "Bucket name must be 3–63 characters", bucket));
    }
    if bucket.starts_with('.') || bucket.ends_with('.') || bucket.starts_with('-') || bucket.ends_with('-') {
        return Err(s3_error(StatusCode::BAD_REQUEST, "InvalidBucketName",
                            "Bucket name cannot start or end with . or -", bucket));
    }
    if bucket.contains("..") || bucket.contains("--") {
        return Err(s3_error(StatusCode::BAD_REQUEST, "InvalidBucketName",
                            "Bucket name cannot contain consecutive . or -", bucket));
    }
    if !bucket.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-') {
        return Err(s3_error(StatusCode::BAD_REQUEST, "InvalidBucketName",
                            "Bucket name must only contain lowercase letters, numbers, hyphens, or dots", bucket));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// ListBuckets  GET /s3  or  GET /s3/
// ---------------------------------------------------------------------------

pub async fn s3_list_buckets_handler(req: HttpRequest) -> Result<HttpResponse, Error> {
    let auth_result = authenticate_s3_request(&req).await?;
    if !auth_result.bucket.is_empty() {
        return Ok(s3_error(StatusCode::BAD_REQUEST, "InvalidRequest",
                           "Unexpected bucket in path for list-buckets", "/"));
    }
    info!("S3 ListBuckets: user={}", auth_result.user_id);

    let db = MetadataService::new(&auth_result.user_id)?;

    // For admin (allow_all_buckets), pull the full list from the buckets table so that
    // empty buckets are included. For Console users, restrict to the allowed set.
    let names: Vec<String> = if auth_result.allow_all_buckets {
        db.list_all_buckets()?
    } else {
        auth_result.allowed_buckets.clone()
    };

    // Overlay stats (size + count) for each registered bucket.
    let stats_map: HashMap<String, (u64, u64)> = db
        .list_buckets_with_stats()?
        .into_iter()
        .map(|s| (s.name, (s.object_count, s.total_size)))
        .collect();

    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S.000Z");
    let mut buckets_xml = String::new();
    for name in &names {
        let (count, size) = stats_map.get(name).copied().unwrap_or((0, 0));
        buckets_xml.push_str(&format!(
            "    <Bucket>\n\
             \t<Name>{name}</Name>\n\
             \t<CreationDate>{now}</CreationDate>\n\
             \t<ObjectCount xmlns=\"{ext}\">{count}</ObjectCount>\n\
             \t<TotalSize xmlns=\"{ext}\">{size}</TotalSize>\n\
             \t</Bucket>\n",
            name = xml_escape(name),
            now  = now,
            ext  = WARPDRIVE_LIST_BUCKETS_EXT_NS,
            count = count,
            size  = size,
        ));
    }

    let owner_id = xml_escape(&auth_result.user_id);
    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <ListAllMyBucketsResult xmlns=\"{s3}\">\n\
             <Owner><ID>{owner}</ID><DisplayName>{owner}</DisplayName></Owner>\n\
             <Buckets>\n{buckets}</Buckets>\n\
         </ListAllMyBucketsResult>",
        s3 = S3_XMLNS, owner = owner_id, buckets = buckets_xml,
    );
    Ok(HttpResponse::Ok().content_type("application/xml").body(xml))
}

// ---------------------------------------------------------------------------
// CreateBucket  PUT /s3/{bucket}
// ---------------------------------------------------------------------------

pub async fn s3_create_bucket_handler(
    path: web::Path<String>,
    req: HttpRequest,
) -> Result<HttpResponse, Error> {
    let bucket = path.into_inner();
    let auth_result = authenticate_s3_request(&req).await?;

    if let Err(e) = validate_bucket_name(&bucket) { return Ok(e); }

    let db = MetadataService::new(&auth_result.user_id)?;
    db.create_bucket(&bucket)?;

    info!("S3 CreateBucket: bucket={} user={}", bucket, auth_result.user_id);
    Ok(HttpResponse::Ok()
        .insert_header(("Location", format!("/{}", bucket)))
        .insert_header(("Content-Length", "0"))
        .body(""))
}

// ---------------------------------------------------------------------------
// DeleteBucket  DELETE /s3/{bucket}
// ---------------------------------------------------------------------------

pub async fn s3_delete_bucket_handler(
    path: web::Path<String>,
    req: HttpRequest,
) -> Result<HttpResponse, Error> {
    let bucket = path.into_inner();
    let auth_result = authenticate_s3_request(&req).await?;
    let db = MetadataService::new(&auth_result.user_id)?;

    if let Err(resp) = require_bucket(&db, &bucket) { return Ok(resp); }

    let objects = db.list_objects(&bucket)?;
    if !objects.is_empty() {
        return Ok(s3_error(StatusCode::CONFLICT, "BucketNotEmpty",
                           "The bucket you tried to delete is not empty", &bucket));
    }

    db.delete_bucket(&bucket)?;
    info!("S3 DeleteBucket: bucket={} user={}", bucket, auth_result.user_id);
    Ok(HttpResponse::NoContent().insert_header(("Content-Length", "0")).body(""))
}

// ---------------------------------------------------------------------------
// HeadBucket  HEAD /s3/{bucket}
// ---------------------------------------------------------------------------

pub async fn s3_head_bucket_handler(
    path: web::Path<String>,
    req: HttpRequest,
) -> Result<HttpResponse, Error> {
    let bucket = path.into_inner();
    let auth_result = authenticate_s3_request(&req).await?;
    let db = MetadataService::new(&auth_result.user_id)?;

    if let Err(resp) = require_bucket(&db, &bucket) { return Ok(resp); }

    Ok(HttpResponse::Ok()
        .insert_header(("Content-Type", "application/xml"))
        .insert_header(("Content-Length", "0"))
        .body(""))
}

// ---------------------------------------------------------------------------
// PutObject  PUT /s3/{bucket}/{key}
// ---------------------------------------------------------------------------

pub async fn s3_put_object_handler(
    path: web::Path<(String, String)>,
    mut payload: web::Payload,
    req: HttpRequest,
) -> Result<HttpResponse, Error> {
    // Dispatch sub-operations
    if req.headers().contains_key("x-amz-copy-source") {
        return s3_copy_object_handler(path, req).await;
    }
    if let Ok(query) = web::Query::<HashMap<String, String>>::from_query(req.query_string()) {
        if query.contains_key("partNumber") && query.contains_key("uploadId") {
            return s3_upload_part_handler(path, query, payload, req).await;
        }
    }

    let (bucket, key) = path.into_inner();
    let auth_result = authenticate_s3_request(&req).await?;
    let _authenticated_req = create_authenticated_request(&req, &auth_result);

    let db = MetadataService::new(&auth_result.user_id)?;
    if let Err(resp) = require_bucket(&db, &bucket) { return Ok(resp); }

    info!("S3 PutObject: bucket={} key={} user={}", bucket, key, auth_result.user_id);

    let context = UserContext::with_bucket(auth_result.user_id.clone(), auth_result.bucket.clone());

    // Delete existing object first (S3 PUT is idempotent / overwrites).
    if db.check_key(&bucket, &key)? {
        StorageService::new().delete_object(&context, &key)?;
    }

    // Parse user metadata from x-amz-meta-* headers.
    let user_metadata: HashMap<String, String> = req.headers().iter()
        .filter_map(|(name, value)| {
            let n = name.as_str().to_lowercase();
            if n.starts_with("x-amz-meta-") {
                let meta_key = n.trim_start_matches("x-amz-meta-").to_string();
                let meta_val = value.to_str().ok()?.to_string();
                Some((meta_key, meta_val))
            } else {
                None
            }
        })
        .collect();

    let content_type = req.headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();

    let store = StorageConfig::from_env().create_store();
    let mut offset_size_list: Vec<(u64, u64)> = Vec::new();
    let mut body_buf: Vec<u8> = Vec::new(); // for MD5

    while let Some(chunk_result) = payload.next().await {
        let chunk = chunk_result.map_err(|e| {
            warn!("PutObject: payload read error: {}", e);
            actix_web::error::ErrorInternalServerError("Error reading payload")
        })?;
        if chunk.is_empty() { continue; }

        body_buf.extend_from_slice(&chunk);

        let ctx = context.clone();
        let store_c = Arc::clone(&store);
        let buf = chunk.to_vec();
        let pair = web::block(move || {
            store_c.write(&ctx.user_id, &ctx.bucket, &buf).map_err(|e| e.to_string())
        }).await
        .map_err(|e| {
            error!("PutObject: blocking write failed bucket={} key={}: {:?}", bucket, key, e);
            actix_web::error::ErrorInternalServerError("storage write task failed")
        })?
        .map_err(|msg| {
            error!("PutObject: write error bucket={} key={}: {}", bucket, key, msg);
            actix_web::error::ErrorInternalServerError(msg)
        })?;
        offset_size_list.push(pair);
    }

    let size = body_buf.len() as u64;
    let etag = md5_etag(&body_buf);
    let last_modified = rfc2616_now();

    let mut metadata = Metadata::from_offset_size_list(offset_size_list);
    metadata.etag = Some(etag.clone());
    metadata.size = size;
    metadata.content_type = Some(content_type);
    metadata.last_modified = Some(last_modified);
    metadata.user_metadata = user_metadata;

    db.put_object_full(&bucket, &key, metadata)?;

    debug!("S3 PutObject OK: bucket={} key={} size={} etag={}", bucket, key, size, etag);
    Ok(HttpResponse::Ok()
        .insert_header(("ETag", etag))
        .insert_header(("Content-Length", "0"))
        .body(""))
}

// ---------------------------------------------------------------------------
// GetObject  GET /s3/{bucket}/{key}
// ---------------------------------------------------------------------------

pub async fn s3_get_object_handler(
    path: web::Path<(String, String)>,
    req: HttpRequest,
) -> Result<HttpResponse, Error> {
    let (bucket, key) = path.into_inner();
    let auth_result = authenticate_s3_request(&req).await?;
    let _authenticated_req = create_authenticated_request(&req, &auth_result);

    let db = MetadataService::new(&auth_result.user_id)?;
    if let Err(resp) = require_bucket(&db, &bucket) { return Ok(resp); }

    if !db.check_key(&bucket, &key)? {
        return Ok(s3_error(StatusCode::NOT_FOUND, "NoSuchKey",
                           "The specified key does not exist", &format!("/{}/{}", bucket, key)));
    }

    let meta = db.get_object_full(&bucket, &key)?;
    let slices = Arc::new(stream_slices(&meta.to_offset_size_list()));
    let content_length = meta.size;
    let etag = meta.etag.clone().unwrap_or_default();
    let content_type = meta.content_type.clone().unwrap_or_else(|| "application/octet-stream".into());
    let last_modified = meta.last_modified.clone().unwrap_or_default();

    info!("S3 GetObject: bucket={} key={} size={}", bucket, key, content_length);

    let store = StorageConfig::from_env().create_store();
    let context = UserContext::with_bucket(auth_result.user_id.clone(), auth_result.bucket.clone());
    let bucket_log = bucket.clone();
    let key_log = key.clone();

    let byte_stream = stream::try_unfold(0usize, move |idx| {
        let slices = Arc::clone(&slices);
        let ctx = context.clone();
        let bucket_log = bucket_log.clone();
        let key_log = key_log.clone();
        let store = Arc::clone(&store);
        async move {
            if idx >= slices.len() {
                return Ok::<Option<(Bytes, usize)>, Error>(None);
            }
            let (off, sz) = slices[idx];
            let chunk = web::block(move || {
                store.read(&ctx.user_id, &ctx.bucket, off, sz).map_err(|e| e.to_string())
            }).await
            .map_err(|e| {
                error!("GetObject stream: task failed bucket={} key={} idx={}: {:?}", bucket_log, key_log, idx, e);
                actix_web::error::ErrorInternalServerError(e)
            })?
            .map_err(|msg| {
                error!("GetObject stream: read error bucket={} key={} idx={}: {}", bucket_log, key_log, idx, msg);
                actix_web::error::ErrorInternalServerError(msg)
            })?;
            Ok(Some((Bytes::from(chunk), idx + 1)))
        }
    });

    let mut resp = HttpResponse::Ok();
    resp.content_type(content_type.as_str());
    resp.insert_header(("Content-Length", content_length.to_string()));
    resp.insert_header(("ETag", etag));
    if !last_modified.is_empty() {
        resp.insert_header(("Last-Modified", last_modified));
    }
    // Echo user metadata
    for (k, v) in &meta.user_metadata {
        resp.insert_header((format!("x-amz-meta-{}", k).as_str(), v.as_str()));
    }
    Ok(resp.streaming(byte_stream))
}

// ---------------------------------------------------------------------------
// HeadObject  HEAD /s3/{bucket}/{key}
// ---------------------------------------------------------------------------

pub async fn s3_head_object_handler(
    path: web::Path<(String, String)>,
    req: HttpRequest,
) -> Result<HttpResponse, Error> {
    let (bucket, key) = path.into_inner();
    let auth_result = authenticate_s3_request(&req).await?;

    let db = MetadataService::new(&auth_result.user_id)?;
    if let Err(resp) = require_bucket(&db, &bucket) { return Ok(resp); }

    if !db.check_key(&bucket, &key)? {
        return Ok(s3_error(StatusCode::NOT_FOUND, "NoSuchKey",
                           "The specified key does not exist", &format!("/{}/{}", bucket, key)));
    }

    let meta = db.get_object_full(&bucket, &key)?;
    let etag = meta.etag.clone().unwrap_or_default();
    let content_type = meta.content_type.clone().unwrap_or_else(|| "application/octet-stream".into());
    let last_modified = meta.last_modified.clone().unwrap_or_default();

    info!("S3 HeadObject: bucket={} key={} size={}", bucket, key, meta.size);

    let mut resp = HttpResponse::Ok();
    resp.insert_header(("Content-Length", meta.size.to_string()));
    resp.insert_header(("Content-Type", content_type));
    resp.insert_header(("ETag", etag));
    if !last_modified.is_empty() {
        resp.insert_header(("Last-Modified", last_modified));
    }
    for (k, v) in &meta.user_metadata {
        resp.insert_header((format!("x-amz-meta-{}", k).as_str(), v.as_str()));
    }
    Ok(resp.body(""))
}

// ---------------------------------------------------------------------------
// DeleteObject  DELETE /s3/{bucket}/{key}
// ---------------------------------------------------------------------------

pub async fn s3_delete_object_handler(
    path: web::Path<(String, String)>,
    req: HttpRequest,
) -> Result<HttpResponse, Error> {
    // Dispatch abort-multipart
    if let Ok(query) = web::Query::<HashMap<String, String>>::from_query(req.query_string()) {
        if query.contains_key("uploadId") {
            return s3_abort_multipart_upload_handler(path, query, req).await;
        }
    }

    let (bucket, key) = path.into_inner();
    let auth_result = authenticate_s3_request(&req).await?;
    let _authenticated_req = create_authenticated_request(&req, &auth_result);

    let context = UserContext::with_bucket(auth_result.user_id.clone(), auth_result.bucket.clone());
    let db = MetadataService::new(&auth_result.user_id)?;

    if let Err(resp) = require_bucket(&db, &bucket) { return Ok(resp); }

    // S3 DELETE is idempotent — 204 even if the key didn't exist.
    if db.check_key(&bucket, &key)? {
        StorageService::new().delete_object(&context, &key)?;
        info!("S3 DeleteObject: bucket={} key={}", bucket, key);
    } else {
        debug!("S3 DeleteObject: bucket={} key={} (not found, returning 204)", bucket, key);
    }

    Ok(HttpResponse::NoContent()
        .insert_header(("Content-Length", "0"))
        .body(""))
}

// ---------------------------------------------------------------------------
// ListObjects  GET /s3/{bucket}
// ---------------------------------------------------------------------------

pub async fn s3_list_objects_handler(
    path: web::Path<String>,
    query: web::Query<HashMap<String, String>>,
    req: HttpRequest,
) -> Result<HttpResponse, Error> {
    let bucket = path.into_inner();
    let auth_result = authenticate_s3_request(&req).await?;
    let db = MetadataService::new(&auth_result.user_id)?;

    if let Err(resp) = require_bucket(&db, &bucket) { return Ok(resp); }

    info!("S3 ListObjects: bucket={}", bucket);

    let objects = db.list_objects(&bucket)?;
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S.000Z");

    // Support both V1 (no list-type) and V2 (list-type=2) with basic response.
    // Full prefix/delimiter/pagination support is Batch 2.
    let list_type = query.get("list-type").map(|s| s.as_str()).unwrap_or("1");
    let is_v2 = list_type == "2";

    let mut contents_xml = String::new();
    for object_key in &objects {
        contents_xml.push_str(&format!(
            "    <Contents>\n\
             \t<Key>{key}</Key>\n\
             \t<Size>0</Size>\n\
             \t<LastModified>{now}</LastModified>\n\
             \t<ETag>&quot;&quot;</ETag>\n\
             \t<StorageClass>STANDARD</StorageClass>\n\
             \t</Contents>\n",
            key = xml_escape(object_key),
            now = now,
        ));
    }

    let xml = if is_v2 {
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <ListBucketResult xmlns=\"{s3}\">\n\
                 <Name>{bucket}</Name>\n\
                 <Prefix></Prefix>\n\
                 <KeyCount>{count}</KeyCount>\n\
                 <MaxKeys>1000</MaxKeys>\n\
                 <IsTruncated>false</IsTruncated>\n\
                 {contents}\
             </ListBucketResult>",
            s3 = S3_XMLNS, bucket = xml_escape(&bucket),
            count = objects.len(), contents = contents_xml,
        )
    } else {
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <ListBucketResult xmlns=\"{s3}\">\n\
                 <Name>{bucket}</Name>\n\
                 <Prefix></Prefix>\n\
                 <Marker></Marker>\n\
                 <MaxKeys>1000</MaxKeys>\n\
                 <IsTruncated>false</IsTruncated>\n\
                 {contents}\
             </ListBucketResult>",
            s3 = S3_XMLNS, bucket = xml_escape(&bucket),
            contents = contents_xml,
        )
    };

    Ok(HttpResponse::Ok().content_type("application/xml").body(xml))
}

// ---------------------------------------------------------------------------
// CopyObject  PUT /s3/{dst_bucket}/{dst_key} with x-amz-copy-source header
// ---------------------------------------------------------------------------

pub async fn s3_copy_object_handler(
    path: web::Path<(String, String)>,
    req: HttpRequest,
) -> Result<HttpResponse, Error> {
    let (dst_bucket, dst_key) = path.into_inner();
    let auth_result = authenticate_s3_request(&req).await?;

    let copy_source = match req.headers().get("x-amz-copy-source") {
        Some(h) => h.to_str()
            .map_err(|_| actix_web::error::ErrorBadRequest("Invalid x-amz-copy-source header"))?
            .to_string(),
        None => return Ok(s3_error(StatusCode::BAD_REQUEST, "InvalidArgument",
                                   "Missing x-amz-copy-source header", &dst_bucket)),
    };

    // Source format: /src-bucket/src-key  (leading slash optional, percent-encoded)
    let source = copy_source.trim_start_matches('/');
    let (src_bucket, src_key) = match source.splitn(2, '/').collect::<Vec<_>>().as_slice() {
        [b, k] => (b.to_string(), k.to_string()),
        _ => return Ok(s3_error(StatusCode::BAD_REQUEST, "InvalidArgument",
                                "Invalid x-amz-copy-source format (expected bucket/key)", &dst_bucket)),
    };

    info!("S3 CopyObject: {}/{} → {}/{}", src_bucket, src_key, dst_bucket, dst_key);

    let db = MetadataService::new(&auth_result.user_id)?;
    if let Err(resp) = require_bucket(&db, &src_bucket) { return Ok(resp); }
    if let Err(resp) = require_bucket(&db, &dst_bucket) { return Ok(resp); }

    if !db.check_key(&src_bucket, &src_key)? {
        return Ok(s3_error(StatusCode::NOT_FOUND, "NoSuchKey",
                           "The source key does not exist", &format!("/{}/{}", src_bucket, src_key)));
    }

    let src_meta = db.get_object_full(&src_bucket, &src_key)?;
    let src_context = UserContext::with_bucket(auth_result.user_id.clone(), src_bucket.clone());
    let dst_context = UserContext::with_bucket(auth_result.user_id.clone(), dst_bucket.clone());

    // Read source data and re-write to destination bucket's storage space.
    let storage_service = StorageService::new();
    let src_data = storage_service.read_object(&src_context, &src_meta.to_offset_size_list(), StorageMode::S3)?;
    let new_offset_size_list = storage_service.write_object(&dst_context, &src_data, StorageMode::S3)?;

    // Metadata directive: COPY (default) keeps source metadata; REPLACE uses new request headers.
    let directive = req.headers().get("x-amz-metadata-directive")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("COPY");

    let (content_type, user_metadata) = if directive == "REPLACE" {
        let ct = req.headers().get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_string();
        let um: HashMap<String, String> = req.headers().iter()
            .filter_map(|(name, value)| {
                let n = name.as_str().to_lowercase();
                if n.starts_with("x-amz-meta-") {
                    let k = n.trim_start_matches("x-amz-meta-").to_string();
                    let v = value.to_str().ok()?.to_string();
                    Some((k, v))
                } else { None }
            })
            .collect();
        (ct, um)
    } else {
        (
            src_meta.content_type.clone().unwrap_or_else(|| "application/octet-stream".into()),
            src_meta.user_metadata.clone(),
        )
    };

    let etag = md5_etag(&src_data);
    let last_modified = rfc2616_now();

    // Overwrite destination if it already exists.
    if db.check_key(&dst_bucket, &dst_key)? {
        storage_service.delete_object(&dst_context, &dst_key)?;
    }

    let new_offset_size_bytes = crate::util::serializer::serialize_offset_size(&new_offset_size_list)?;
    let mut dst_meta = Metadata::from_offset_size_list(
        deserialize_offset_size(&new_offset_size_bytes)?
    );
    dst_meta.etag = Some(etag.clone());
    dst_meta.size = src_data.len() as u64;
    dst_meta.content_type = Some(content_type);
    dst_meta.last_modified = Some(last_modified.clone());
    dst_meta.user_metadata = user_metadata;

    db.put_object_full(&dst_bucket, &dst_key, dst_meta)?;

    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <CopyObjectResult xmlns=\"{s3}\">\n\
             <ETag>{etag}</ETag>\n\
             <LastModified>{lm}</LastModified>\n\
         </CopyObjectResult>",
        s3 = S3_XMLNS, etag = xml_escape(&etag), lm = last_modified,
    );
    Ok(HttpResponse::Ok().content_type("application/xml").body(xml))
}

// ---------------------------------------------------------------------------
// Multipart upload handlers (existing logic preserved, bucket-check added)
// ---------------------------------------------------------------------------

pub async fn s3_create_multipart_upload_handler(
    path: web::Path<(String, String)>,
    query: web::Query<HashMap<String, String>>,
    req: HttpRequest,
) -> Result<HttpResponse, Error> {
    let (bucket, key) = path.into_inner();
    if query.get("uploads").map(|s| s.as_str()) != Some("") {
        return Ok(s3_error(StatusCode::BAD_REQUEST, "InvalidRequest",
                           "Invalid multipart upload initiation request", &bucket));
    }

    let auth_result = authenticate_s3_request(&req).await?;
    let db = MetadataService::new(&auth_result.user_id)?;
    if let Err(resp) = require_bucket(&db, &bucket) { return Ok(resp); }

    let upload_id = format!("upload_{}", chrono::Utc::now().timestamp_millis());
    info!("S3 CreateMultipartUpload: bucket={} key={} uploadId={}", bucket, key, upload_id);

    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <InitiateMultipartUploadResult xmlns=\"{s3}\">\n\
             <Bucket>{bucket}</Bucket>\n\
             <Key>{key}</Key>\n\
             <UploadId>{uid}</UploadId>\n\
         </InitiateMultipartUploadResult>",
        s3 = S3_XMLNS,
        bucket = xml_escape(&bucket),
        key = xml_escape(&key),
        uid = upload_id,
    );
    Ok(HttpResponse::Ok().content_type("application/xml").body(xml))
}

pub async fn s3_upload_part_handler(
    path: web::Path<(String, String)>,
    query: web::Query<HashMap<String, String>>,
    mut payload: web::Payload,
    req: HttpRequest,
) -> Result<HttpResponse, Error> {
    let (_bucket, key) = path.into_inner();
    let part_number = query.get("partNumber")
        .ok_or_else(|| actix_web::error::ErrorBadRequest("Missing partNumber"))?
        .clone();
    let upload_id = query.get("uploadId")
        .ok_or_else(|| actix_web::error::ErrorBadRequest("Missing uploadId"))?
        .clone();

    let auth_result = authenticate_s3_request(&req).await?;

    let mut body: Vec<u8> = Vec::new();
    while let Some(chunk) = payload.next().await {
        let chunk = chunk.map_err(|e| {
            warn!("UploadPart: read error: {}", e);
            actix_web::error::ErrorInternalServerError("Error reading payload")
        })?;
        body.extend_from_slice(&chunk);
    }

    if body.is_empty() {
        return Ok(s3_error(StatusCode::BAD_REQUEST, "InvalidPart",
                           "Part must not be empty", &key));
    }

    let context = UserContext::with_bucket(auth_result.user_id.clone(), auth_result.bucket.clone());
    let db = MetadataService::new(&auth_result.user_id)?;

    let storage_service = StorageService::new();
    let offset_size_list = storage_service.write_object(&context, &body, StorageMode::S3)?;
    let offset_size_bytes = crate::util::serializer::serialize_offset_size(&offset_size_list)?;

    let part_key = format!("{}.part.{}.{}", key, part_number, upload_id);
    db.write_metadata(&auth_result.bucket, &part_key, &offset_size_bytes)?;

    let etag = format!("\"{}\"", hex::encode(md5::compute(&body).0));
    Ok(HttpResponse::Ok().insert_header(("ETag", etag)).body(""))
}

pub async fn s3_complete_multipart_upload_handler(
    path: web::Path<(String, String)>,
    query: web::Query<HashMap<String, String>>,
    mut payload: web::Payload,
    req: HttpRequest,
) -> Result<HttpResponse, Error> {
    let (bucket, key) = path.into_inner();
    let upload_id = query.get("uploadId")
        .ok_or_else(|| actix_web::error::ErrorBadRequest("Missing uploadId"))?
        .clone();

    let auth_result = authenticate_s3_request(&req).await?;
    let db = MetadataService::new(&auth_result.user_id)?;
    if let Err(resp) = require_bucket(&db, &bucket) { return Ok(resp); }

    let mut bytes: Vec<u8> = Vec::new();
    while let Some(chunk) = payload.next().await {
        let chunk = chunk.map_err(|e| {
            warn!("CompleteMultipartUpload: read error: {}", e);
            actix_web::error::ErrorInternalServerError("Error reading payload")
        })?;
        bytes.extend_from_slice(&chunk);
    }

    let context = UserContext::with_bucket(auth_result.user_id.clone(), auth_result.bucket.clone());

    // Collect and sort part keys.
    let all_objects = db.list_objects(&bucket)?;
    let mut parts: Vec<(i32, String)> = Vec::new();
    for obj_key in all_objects {
        if obj_key.starts_with(&format!("{}.part.", key)) && obj_key.ends_with(&format!(".{}", upload_id)) {
            let part_info = obj_key
                .strip_prefix(&format!("{}.part.", key))
                .and_then(|s| s.strip_suffix(&format!(".{}", upload_id)))
                .and_then(|s| s.parse::<i32>().ok());
            if let Some(part_number) = part_info {
                parts.push((part_number, obj_key));
            }
        }
    }
    parts.sort_by_key(|(n, _)| *n);
    if parts.is_empty() {
        return Ok(s3_error(StatusCode::BAD_REQUEST, "InvalidPart",
                           "No uploaded parts found for uploadId", &bucket));
    }

    // Concatenate part extent lists into the final object manifest.
    let mut final_extents: Vec<(u64, u64)> = Vec::new();
    for (_, part_key) in &parts {
        let part_bytes = db.read_metadata(&bucket, part_key)?;
        let part_extents = deserialize_offset_size(&part_bytes)?;
        final_extents.extend(part_extents);
    }

    if db.check_key(&bucket, &key)? {
        StorageService::new().delete_object(&context, &key)?;
    }

    let final_bytes = crate::util::serializer::serialize_offset_size(&final_extents)?;
    db.write_metadata(&bucket, &key, &final_bytes)?;

    for (_, part_key) in &parts {
        db.delete_metadata(&bucket, part_key)?;
    }

    info!("S3 CompleteMultipartUpload: bucket={} key={} parts={}", bucket, key, parts.len());

    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <CompleteMultipartUploadResult xmlns=\"{s3}\">\n\
             <Location>http://{bucket}.s3.amazonaws.com/{key}</Location>\n\
             <Bucket>{bucket}</Bucket>\n\
             <Key>{key}</Key>\n\
             <ETag>&quot;s3-etag-multipart&quot;</ETag>\n\
         </CompleteMultipartUploadResult>",
        s3 = S3_XMLNS,
        bucket = xml_escape(&bucket),
        key = xml_escape(&key),
    );
    Ok(HttpResponse::Ok().content_type("application/xml").body(xml))
}

pub async fn s3_abort_multipart_upload_handler(
    path: web::Path<(String, String)>,
    query: web::Query<HashMap<String, String>>,
    req: HttpRequest,
) -> Result<HttpResponse, Error> {
    let (bucket, key) = path.into_inner();
    let upload_id = query.get("uploadId")
        .ok_or_else(|| actix_web::error::ErrorBadRequest("Missing uploadId"))?
        .clone();

    let auth_result = authenticate_s3_request(&req).await?;
    let db = MetadataService::new(&auth_result.user_id)?;
    if let Err(resp) = require_bucket(&db, &bucket) { return Ok(resp); }

    let context = UserContext::with_bucket(auth_result.user_id.clone(), auth_result.bucket.clone());
    let all_objects = db.list_objects(&bucket)?;
    for obj_key in all_objects {
        if obj_key.starts_with(&format!("{}.part.", key)) && obj_key.ends_with(&format!(".{}", upload_id)) {
            StorageService::new().delete_object(&context, &obj_key)?;
        }
    }

    info!("S3 AbortMultipartUpload: bucket={} key={} uploadId={}", bucket, key, upload_id);
    Ok(HttpResponse::NoContent()
        .insert_header(("Content-Length", "0"))
        .body(""))
}

pub async fn s3_multipart_router(
    path: web::Path<(String, String)>,
    query: web::Query<HashMap<String, String>>,
    payload: web::Payload,
    req: HttpRequest,
) -> Result<HttpResponse, Error> {
    if query.contains_key("uploads") {
        s3_create_multipart_upload_handler(path, query, req).await
    } else if query.contains_key("uploadId") {
        s3_complete_multipart_upload_handler(path, query, payload, req).await
    } else {
        Ok(s3_error(StatusCode::BAD_REQUEST, "InvalidRequest", "Invalid multipart operation", ""))
    }
}
