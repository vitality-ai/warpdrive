// S3-compatible request handlers
use actix_web::{web, HttpRequest, HttpResponse, Error, http::StatusCode};
use actix_web::body::{BodySize, MessageBody};
use log::{debug, error, info, warn};
use bytes::Bytes;
use futures::stream::{self, StreamExt as _};

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

/// Empty body that reports a custom Content-Length for HEAD responses.
/// actix-web derives Content-Length from MessageBody::size(); this type
/// lets us advertise the real object size without sending any bytes.
struct HeadBody(u64);
impl MessageBody for HeadBody {
    type Error = std::convert::Infallible;
    fn size(&self) -> BodySize { BodySize::Sized(self.0) }
    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Result<Bytes, Self::Error>>> {
        Poll::Ready(None)
    }
}

use crate::metadata::Metadata;
use crate::s3::auth::{authenticate_s3_request, create_authenticated_request};
use crate::service::metadata_service::MetadataService;
use crate::service::user_context::UserContext;
use crate::service::storage_service::{StorageService, StorageMode};
use crate::storage::config::StorageConfig;
use crate::util::serializer::deserialize_offset_size;

const S3_XMLNS: &str = "http://s3.amazonaws.com/doc/2006-03-01/";
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

/// Encode a stored metadata value as a HeaderValue for HTTP responses.
/// boto3/urllib3 1.x decodes response headers as Latin-1 (ISO-8859-1). Metadata values
/// are stored internally as UTF-8 Rust Strings; we encode each Unicode code point to its
/// low byte (safe for the Latin-1 range U+0000..U+00FF) so the round-trip is lossless
/// for any character that was originally in the Latin-1 range.
fn metadata_value_header(v: &str) -> actix_web::http::header::HeaderValue {
    let bytes: Vec<u8> = v.chars()
        .map(|c| if (c as u32) <= 0xFF { c as u8 } else { b'?' })
        .collect();
    actix_web::http::header::HeaderValue::from_bytes(&bytes)
        .unwrap_or_else(|_| actix_web::http::header::HeaderValue::from_static(""))
}

/// S3 URL-encoding for listing responses: percent-encode all bytes except unreserved chars and '/'.
fn s3_url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' |
            b'-' | b'_' | b'.' | b'~' | b'/' => out.push(byte as char),
            b => { out.push('%'); out.push_str(&format!("{:02X}", b)); }
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
    HttpResponse::build(status)
        .content_type("application/xml")
        .insert_header(("x-amz-request-id", "warpdrive"))
        .body(body)
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

/// Parse `Range: bytes=X-Y` or `bytes=X-` → (start, end_inclusive) in logical object space.
/// Returns None if the header is absent or unparseable.
fn parse_range_header(req: &HttpRequest, total: u64) -> Option<(u64, u64)> {
    let hdr = req.headers().get("range")?.to_str().ok()?;
    let bytes = hdr.strip_prefix("bytes=")?;
    let (start_s, end_s) = bytes.split_once('-')?;
    let start: u64 = start_s.parse().ok()?;
    let end: u64 = if end_s.is_empty() {
        total.saturating_sub(1)
    } else {
        end_s.parse().ok()?
    };
    if start > end || end >= total { return None; }
    Some((start, end))
}

/// Map a logical byte range [range_start, range_end] onto the storage extents.
/// Returns a list of (storage_offset, read_size) covering exactly the requested bytes.
fn range_slices(chunks: &[(u64, u64)], range_start: u64, range_end: u64) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    let mut logical = 0u64;
    for &(storage_off, chunk_size) in chunks {
        let chunk_end = logical + chunk_size; // exclusive
        if chunk_end <= range_start {
            logical = chunk_end;
            continue;
        }
        if logical >= range_end + 1 {
            break;
        }
        // overlap: [max(logical, range_start), min(chunk_end, range_end+1))
        let read_start = range_start.max(logical);
        let read_end   = (range_end + 1).min(chunk_end);
        let skip       = read_start - logical;
        let read_size  = read_end - read_start;
        let mut off = storage_off + skip;
        let mut rem = read_size;
        while rem > 0 {
            let n = rem.min(S3_GET_STREAM_CHUNK);
            out.push((off, n));
            off += n;
            rem -= n;
        }
        logical = chunk_end;
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

/// Strip surrounding double quotes from an ETag for comparison (handles `"hash"` and `hash`).
#[inline]
fn normalize_etag(etag: &str) -> &str {
    etag.trim().trim_matches('"')
}

/// Return a 412 PreconditionFailed S3 error response.
#[inline]
fn s3_precondition_failed(resource: &str) -> HttpResponse {
    s3_error(StatusCode::PRECONDITION_FAILED, "PreconditionFailed",
             "At least one of the pre-conditions you specified did not hold",
             resource)
}

/// Extract the text content of the first occurrence of `<tag>…</tag>` in `src`.
fn extract_xml_tag(src: &str, tag: &str) -> Option<String> {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);
    let start = src.find(&open)? + open.len();
    let end = src[start..].find(&close)?;
    Some(src[start..start + end].to_string())
}

/// Parse an HTTP date string (RFC 2616 / RFC 1123: "Mon, 01 Jan 2024 00:00:00 GMT")
/// into a comparable integer (Unix seconds).  Returns None if parsing fails.
fn parse_http_date(s: &str) -> Option<i64> {
    // Try RFC 2616 format first: "Mon, 01 Jan 2024 00:00:00 GMT"
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%a, %d %b %Y %H:%M:%S GMT") {
        return Some(dt.and_utc().timestamp());
    }
    // Fallback: RFC 2822 (includes timezone offset)
    if let Ok(dt) = chrono::DateTime::parse_from_rfc2822(s) {
        return Some(dt.timestamp());
    }
    None
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

/// Reject keys containing C0/C1 control characters (matches S3's URI parse rejection).
fn validate_object_key(key: &str, bucket: &str) -> Result<(), HttpResponse> {
    if key.chars().any(|c| {
        let n = c as u32;
        n < 0x20 || (n >= 0x7F && n <= 0x9F)
    }) {
        return Err(s3_error(StatusCode::BAD_REQUEST, "InvalidURI",
                            "Couldn't parse the specified URI.",
                            &format!("/{}/{}", bucket, key)));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// ListBuckets  GET /s3  or  GET /s3/
// ---------------------------------------------------------------------------

pub async fn s3_list_buckets_handler(
    query: web::Query<HashMap<String, String>>,
    req: HttpRequest,
) -> Result<HttpResponse, Error> {
    let auth_result = authenticate_s3_request(&req).await?;
    if !auth_result.bucket.is_empty() {
        return Ok(s3_error(StatusCode::BAD_REQUEST, "InvalidRequest",
                           "Unexpected bucket in path for list-buckets", "/"));
    }
    info!("S3 ListBuckets: user={}", auth_result.user_id);

    let db = MetadataService::new(&auth_result.user_id)?;

    let all_stats = db.list_buckets_with_stats()?;

    // Admin sees all registered buckets; Console users see only allowed ones.
    let allowed_set: Option<std::collections::HashSet<&str>> = if auth_result.allow_all_buckets {
        None
    } else {
        Some(auth_result.allowed_buckets.iter().map(|s| s.as_str()).collect())
    };

    // Pagination params: max-buckets + continuation-token (new ListBuckets API)
    let max_buckets: usize = query.get("max-buckets")
        .and_then(|s| s.parse().ok())
        .unwrap_or(usize::MAX);
    let after: &str = query.get("continuation-token").map(|s| s.as_str()).unwrap_or("");

    let mut buckets_xml = String::new();
    let mut count = 0usize;
    let mut last_name = String::new();
    let mut truncated = false;

    for stat in &all_stats {
        if let Some(ref allowed) = allowed_set {
            if !allowed.contains(stat.name.as_str()) { continue; }
        }
        if !after.is_empty() && stat.name.as_str() <= after {
            continue;
        }
        if count >= max_buckets {
            truncated = true;
            break;
        }
        last_name = stat.name.clone();
        count += 1;
        buckets_xml.push_str(&format!(
            "    <Bucket>\n\
             \t<Name>{name}</Name>\n\
             \t<CreationDate>{ctime}</CreationDate>\n\
             \t</Bucket>\n",
            name  = xml_escape(&stat.name),
            ctime = xml_escape(&stat.created_at),
        ));
    }

    let continuation_xml = if truncated {
        format!("    <ContinuationToken>{}</ContinuationToken>\n", xml_escape(&last_name))
    } else {
        String::new()
    };

    let owner_id = xml_escape(&auth_result.user_id);
    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <ListAllMyBucketsResult xmlns=\"{s3}\">\n\
             <Owner><ID>{owner}</ID><DisplayName>{owner}</DisplayName></Owner>\n\
             <Buckets>\n{buckets}</Buckets>\n\
             {continuation}\
         </ListAllMyBucketsResult>",
        s3 = S3_XMLNS, owner = owner_id, buckets = buckets_xml,
        continuation = continuation_xml,
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

    let mut resp = HttpResponse::Ok();
    resp.insert_header(("Content-Type", "application/xml"));
    resp.insert_header(("Content-Length", "0"));

    // Return bucket stats when ?read-stats=true is requested (RGW extension).
    let query = web::Query::<HashMap<String, String>>::from_query(req.query_string())
        .unwrap_or_else(|_| web::Query(HashMap::new()));
    if query.contains_key("read-stats") {
        if let Ok((count, bytes)) = db.bucket_object_stats(&bucket) {
            resp.insert_header(("x-rgw-object-count", count.to_string()));
            resp.insert_header(("x-rgw-bytes-used", bytes.to_string()));
        }
    }

    Ok(resp.body(""))
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

    // Conditional PUT: check If-Match / If-None-Match before overwriting.
    let if_match_put = req.headers().get("if-match")
        .and_then(|v| v.to_str().ok()).map(|s| s.trim().to_string());
    let if_none_match_put = req.headers().get("if-none-match")
        .and_then(|v| v.to_str().ok()).map(|s| s.trim().to_string());
    if if_match_put.is_some() || if_none_match_put.is_some() {
        let resource = format!("/{}/{}", bucket, key);
        let obj_exists = db.check_key(&bucket, &key)?;
        let cur_etag = if obj_exists {
            db.get_object_full(&bucket, &key)?.etag.unwrap_or_default()
        } else {
            String::new()
        };
        if let Some(ref im) = if_match_put {
            if !obj_exists {
                return Ok(s3_error(StatusCode::NOT_FOUND, "NoSuchKey",
                                   "The specified key does not exist.", &resource));
            }
            if im != "*" && normalize_etag(im) != normalize_etag(&cur_etag) {
                return Ok(s3_precondition_failed(&resource));
            }
        }
        if let Some(ref inm) = if_none_match_put {
            if obj_exists && (inm == "*" || normalize_etag(inm) == normalize_etag(&cur_etag)) {
                return Ok(s3_precondition_failed(&resource));
            }
        }
    }

    // Delete existing object first (S3 PUT is idempotent / overwrites).
    if db.check_key(&bucket, &key)? {
        StorageService::new().delete_object(&context, &key)?;
    }

    // Parse user metadata from x-amz-meta-* headers. Use from_utf8_lossy so non-ASCII
    // values (e.g. UTF-8 user metadata sent by boto3) are preserved rather than dropped.
    let user_metadata: HashMap<String, String> = req.headers().iter()
        .filter_map(|(name, value)| {
            let n = name.as_str().to_lowercase();
            if n.starts_with("x-amz-meta-") {
                let meta_key = n.trim_start_matches("x-amz-meta-").to_string();
                let meta_val = String::from_utf8_lossy(value.as_bytes()).trim().to_string();
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

    let cache_control = req.headers()
        .get("cache-control")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let expires = req.headers()
        .get("expires")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

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

    // Validate Content-MD5 if provided (RFC 1864 / S3 spec).
    if let Some(raw) = req.headers().get("content-md5").and_then(|v| v.to_str().ok()) {
        let raw = raw.trim();
        if raw.is_empty() {
            return Ok(s3_error(StatusCode::BAD_REQUEST, "InvalidDigest",
                               "The Content-MD5 you specified is not valid.",
                               &format!("/{}/{}", bucket, key)));
        }
        use base64::Engine as _;
        match base64::engine::general_purpose::STANDARD.decode(raw) {
            Ok(decoded) if decoded.len() == 16 => {
                let body_md5 = md5::compute(&body_buf).0;
                if decoded.as_slice() != &body_md5 {
                    return Ok(s3_error(StatusCode::BAD_REQUEST, "BadDigest",
                                       "The Content-MD5 you specified did not match what we received.",
                                       &format!("/{}/{}", bucket, key)));
                }
            }
            _ => {
                return Ok(s3_error(StatusCode::BAD_REQUEST, "InvalidDigest",
                                   "The Content-MD5 you specified is not valid.",
                                   &format!("/{}/{}", bucket, key)));
            }
        }
    }

    let mut metadata = Metadata::from_offset_size_list(offset_size_list);
    metadata.etag = Some(etag.clone());
    metadata.size = size;
    metadata.content_type = Some(content_type);
    metadata.last_modified = Some(last_modified);
    metadata.user_metadata = user_metadata;
    metadata.cache_control = cache_control;
    metadata.expires = expires;

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

    if let Err(resp) = validate_object_key(&key, &bucket) { return Ok(resp); }

    let auth_result = authenticate_s3_request(&req).await?;
    let _authenticated_req = create_authenticated_request(&req, &auth_result);

    let db = MetadataService::new(&auth_result.user_id)?;
    if let Err(resp) = require_bucket(&db, &bucket) { return Ok(resp); }

    if !db.check_key(&bucket, &key)? {
        return Ok(s3_error(StatusCode::NOT_FOUND, "NoSuchKey",
                           "The specified key does not exist", &format!("/{}/{}", bucket, key)));
    }

    let meta = db.get_object_full(&bucket, &key)?;
    let total_size = meta.size;
    let etag = meta.etag.clone().unwrap_or_default();
    let content_type = meta.content_type.clone().unwrap_or_else(|| "application/octet-stream".into());
    let last_modified = meta.last_modified.clone().unwrap_or_default();
    let extents = meta.to_offset_size_list();

    // Conditional GET: evaluate If-Match, If-None-Match, If-Unmodified-Since, If-Modified-Since.
    {
        let get_if_match = req.headers().get("if-match")
            .and_then(|v| v.to_str().ok()).map(|s| s.trim().to_string());
        let get_if_none_match = req.headers().get("if-none-match")
            .and_then(|v| v.to_str().ok()).map(|s| s.trim().to_string());
        let get_if_modified = req.headers().get("if-modified-since")
            .and_then(|v| v.to_str().ok()).map(|s| s.trim().to_string());
        let get_if_unmodified = req.headers().get("if-unmodified-since")
            .and_then(|v| v.to_str().ok()).map(|s| s.trim().to_string());
        let resource = format!("/{}/{}", bucket, key);

        // If-Match: 412 if current etag doesn't match.
        if let Some(ref im) = get_if_match {
            if im != "*" && normalize_etag(im) != normalize_etag(&etag) {
                return Ok(s3_precondition_failed(&resource));
            }
        }
        // If-Unmodified-Since: 412 if object was modified after the given date.
        if let Some(ref ius) = get_if_unmodified {
            if let (Some(hdr_ts), Some(obj_ts)) =
                (parse_http_date(ius), parse_http_date(&last_modified))
            {
                if obj_ts > hdr_ts {
                    return Ok(s3_precondition_failed(&resource));
                }
            }
        }
        // If-None-Match: 304 if current etag matches.
        if let Some(ref inm) = get_if_none_match {
            if inm == "*" || normalize_etag(inm) == normalize_etag(&etag) {
                let mut r = HttpResponse::NotModified();
                if !etag.is_empty() { r.insert_header(("ETag", etag.as_str())); }
                if !last_modified.is_empty() { r.insert_header(("Last-Modified", last_modified.as_str())); }
                return Ok(r.finish());
            }
        }
        // If-Modified-Since: 304 if object was NOT modified after the given date.
        if let Some(ref ims) = get_if_modified {
            if let (Some(hdr_ts), Some(obj_ts)) =
                (parse_http_date(ims), parse_http_date(&last_modified))
            {
                if obj_ts <= hdr_ts {
                    let mut r = HttpResponse::NotModified();
                    if !etag.is_empty() { r.insert_header(("ETag", etag.as_str())); }
                    if !last_modified.is_empty() { r.insert_header(("Last-Modified", last_modified.as_str())); }
                    return Ok(r.finish());
                }
            }
        }
    }

    // Resolve Range header if present.
    let (slices, response_len, range_header) = if let Some((rs, re)) = parse_range_header(&req, total_size) {
        let s = range_slices(&extents, rs, re);
        let len = re - rs + 1;
        let hdr = format!("bytes {}-{}/{}", rs, re, total_size);
        (s, len, Some(hdr))
    } else {
        (stream_slices(&extents), total_size, None)
    };

    info!("S3 GetObject: bucket={} key={} total={} response_len={}", bucket, key, total_size, response_len);

    let slices = Arc::new(slices);
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

    let status = if range_header.is_some() { StatusCode::PARTIAL_CONTENT } else { StatusCode::OK };
    let mut resp = HttpResponse::build(status);
    resp.content_type(content_type.as_str());
    resp.insert_header(("Content-Length", response_len.to_string()));
    resp.insert_header(("ETag", etag));
    resp.insert_header(("Accept-Ranges", "bytes"));
    if let Some(cr) = range_header {
        resp.insert_header(("Content-Range", cr));
    }
    if !last_modified.is_empty() {
        resp.insert_header(("Last-Modified", last_modified));
    }
    if let Some(cc) = &meta.cache_control {
        resp.insert_header(("Cache-Control", cc.as_str()));
    }
    if let Some(exp) = &meta.expires {
        resp.insert_header(("Expires", exp.as_str()));
    }
    for (k, v) in &meta.user_metadata {
        resp.insert_header((format!("x-amz-meta-{}", k), metadata_value_header(v)));
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

    if let Err(resp) = validate_object_key(&key, &bucket) { return Ok(resp); }

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

    let object_size = meta.size;
    let mut resp = HttpResponse::Ok();
    resp.insert_header(("Content-Type", content_type));
    resp.insert_header(("ETag", etag));
    resp.insert_header(("Accept-Ranges", "bytes"));
    if !last_modified.is_empty() {
        resp.insert_header(("Last-Modified", last_modified));
    }
    if let Some(cc) = &meta.cache_control {
        resp.insert_header(("Cache-Control", cc.as_str()));
    }
    if let Some(exp) = &meta.expires {
        resp.insert_header(("Expires", exp.as_str()));
    }
    for (k, v) in &meta.user_metadata {
        resp.insert_header((format!("x-amz-meta-{}", k), metadata_value_header(v)));
    }
    // Use HeadBody so actix-web derives Content-Length from the real object
    // size (via MessageBody::size) rather than from the zero-byte body.
    Ok(resp.message_body(HeadBody(object_size)).unwrap().map_into_boxed_body())
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

    // S3 returns 404 NoSuchBucket for anonymous requests to non-existent buckets
    // (before even evaluating auth). Check bucket existence first when no Authorization
    // header is present so we match this behaviour instead of returning 401.
    let has_auth = req.headers().contains_key("authorization")
        || req.query_string().contains("X-Amz-Signature");
    if !has_auth {
        if let Ok(db) = MetadataService::new("admin") {
            if matches!(db.bucket_exists(&bucket), Ok(false)) {
                return Ok(s3_error(StatusCode::NOT_FOUND, "NoSuchBucket",
                                   "The specified bucket does not exist", &bucket));
            }
        }
    }

    let auth_result = authenticate_s3_request(&req).await?;
    let _authenticated_req = create_authenticated_request(&req, &auth_result);

    let context = UserContext::with_bucket(auth_result.user_id.clone(), auth_result.bucket.clone());
    let db = MetadataService::new(&auth_result.user_id)?;

    if let Err(resp) = require_bucket(&db, &bucket) { return Ok(resp); }

    // Conditional DELETE: If-Match, x-amz-if-match-last-modified-time, x-amz-if-match-size
    let if_match_del = req.headers().get("if-match")
        .and_then(|v| v.to_str().ok()).map(|s| s.trim().to_string());
    let if_mtime_del = req.headers().get("x-amz-if-match-last-modified-time")
        .and_then(|v| v.to_str().ok()).map(|s| s.trim().to_string());
    let if_size_del = req.headers().get("x-amz-if-match-size")
        .and_then(|v| v.to_str().ok()).map(|s| s.trim().to_string());
    if (if_match_del.is_some() || if_mtime_del.is_some() || if_size_del.is_some())
        && db.check_key(&bucket, &key)?
    {
        let meta = db.get_object_full(&bucket, &key)?;
        let resource = format!("/{}/{}", bucket, key);
        if let Some(ref im) = if_match_del {
            if im != "*" && normalize_etag(im) != normalize_etag(meta.etag.as_deref().unwrap_or("")) {
                return Ok(s3_precondition_failed(&resource));
            }
        }
        if let Some(ref mtime) = if_mtime_del {
            if mtime != meta.last_modified.as_deref().unwrap_or("") {
                return Ok(s3_precondition_failed(&resource));
            }
        }
        if let Some(ref sz) = if_size_del {
            if sz.parse::<u64>().map(|expected| expected != meta.size).unwrap_or(false) {
                return Ok(s3_precondition_failed(&resource));
            }
        }
    }

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

    // Dispatch list-object-versions (?versions query param)
    if query.contains_key("versions") {
        return s3_list_object_versions_handler_inner(&bucket, &req).await;
    }

    let auth_result = authenticate_s3_request(&req).await?;
    let db = MetadataService::new(&auth_result.user_id)?;

    if let Err(resp) = require_bucket(&db, &bucket) { return Ok(resp); }

    let list_type = query.get("list-type").map(|s| s.as_str()).unwrap_or("1");
    let is_v2 = list_type == "2";

    let prefix    = query.get("prefix").map(|s| s.as_str()).unwrap_or("");
    let delimiter = query.get("delimiter").map(|s| s.as_str()).unwrap_or("");
    let url_encode = query.get("encoding-type").map(|s| s == "url").unwrap_or(false);
    let allow_unordered = query.get("allow-unordered").map(|s| s == "true").unwrap_or(false);

    // allow-unordered + delimiter is invalid (Ceph RGW extension)
    if allow_unordered && !delimiter.is_empty() {
        return Ok(s3_error(StatusCode::BAD_REQUEST, "InvalidArgument",
                           "allow-unordered is not supported with delimiter", &bucket));
    }

    // Validate and parse max-keys
    let max_keys: usize = if let Some(mk) = query.get("max-keys") {
        match mk.parse::<i64>() {
            Ok(n) if n >= 0 => n as usize,
            _ => return Ok(s3_error(StatusCode::BAD_REQUEST, "InvalidArgument",
                                    "Argument maxKeys must be an integer between 0 and 2147483647",
                                    &bucket)),
        }
    } else {
        1000
    };

    // V2: continuation-token overrides start-after for the effective position.
    let is_continuation = is_v2 && query.contains_key("continuation-token");
    let effective_marker: &str = if is_v2 {
        if let Some(ct) = query.get("continuation-token") {
            ct.as_str()
        } else {
            query.get("start-after").map(|s| s.as_str()).unwrap_or("")
        }
    } else {
        query.get("marker").map(|s| s.as_str()).unwrap_or("")
    };

    let fetch_owner = is_v2 && query.get("fetch-owner").map(|s| s == "true").unwrap_or(false);

    info!("S3 ListObjects{}: bucket={} prefix={:?} delim={:?} max_keys={} marker={:?}",
          if is_v2 { "V2" } else { "V1" }, bucket, prefix, delimiter, max_keys, effective_marker);

    let all_keys = db.list_objects(&bucket)?;
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S.000Z").to_string();
    let owner_id = auth_result.user_id.clone();

    // --- Core listing algorithm ---
    // Keys from SQLite are already sorted lexicographically (ORDER BY key).
    let mut contents_xml   = String::new();
    let mut prefixes_xml   = String::new();
    let mut last_common_prefix = String::new();
    let mut last_key       = String::new();
    let mut count          = 0usize;
    let mut truncated      = false;

    if max_keys > 0 {
        'outer: for key in &all_keys {
            let key = key.as_str();

            // Skip keys at or before the effective marker
            if !effective_marker.is_empty() && key <= effective_marker {
                continue;
            }

            // Apply prefix filter
            if !key.starts_with(prefix) {
                continue;
            }

            // Check for delimiter grouping
            if !delimiter.is_empty() {
                let after_prefix = &key[prefix.len()..];
                if let Some(pos) = after_prefix.find(delimiter) {
                    let group = format!("{}{}{}", prefix, &after_prefix[..pos], delimiter);

                    // If the group falls at or before the marker, skip the whole group
                    if !effective_marker.is_empty() && group.as_str() <= effective_marker {
                        continue;
                    }

                    // Deduplicate: skip if we already emitted this common prefix
                    if group == last_common_prefix {
                        continue;
                    }

                    // A new common prefix counts as 1 toward max_keys
                    if count >= max_keys {
                        truncated = true;
                        break 'outer;
                    }

                    last_common_prefix = group.clone();
                    last_key = group.clone();
                    count += 1;

                    let disp = if url_encode { s3_url_encode(&group) } else { xml_escape(&group) };
                    prefixes_xml.push_str(&format!(
                        "    <CommonPrefixes><Prefix>{}</Prefix></CommonPrefixes>\n", disp
                    ));
                    continue;
                }
            }

            // Regular content item
            if count >= max_keys {
                truncated = true;
                break;
            }

            last_key = key.to_string();
            count += 1;

            let meta = db.get_object_full(&bucket, key).ok();
            let size = meta.as_ref().map(|m| m.size).unwrap_or(0);
            let etag = meta.as_ref().and_then(|m| m.etag.clone()).unwrap_or_default();
            let lm   = meta.as_ref().and_then(|m| m.last_modified.clone())
                           .unwrap_or_else(|| now.clone());

            let disp_key = if url_encode { s3_url_encode(key) } else { xml_escape(key) };

            // V1 always includes <Owner>; V2 only when fetch-owner=true
            let owner_xml = if !is_v2 || fetch_owner {
                format!("      <Owner><ID>{id}</ID><DisplayName>{id}</DisplayName></Owner>\n",
                        id = xml_escape(&owner_id))
            } else {
                String::new()
            };

            contents_xml.push_str(&format!(
                "    <Contents>\n\
                 \t<Key>{key}</Key>\n\
                 \t<LastModified>{lm}</LastModified>\n\
                 \t<ETag>&quot;{etag}&quot;</ETag>\n\
                 \t<Size>{size}</Size>\n\
                 \t<StorageClass>STANDARD</StorageClass>\n\
                 {owner}\
                 \t</Contents>\n",
                key   = disp_key,
                lm    = lm,
                etag  = etag.trim_matches('"'),
                size  = size,
                owner = owner_xml,
            ));
        }
    }

    // Optional fields present only when the corresponding parameter was supplied
    let delimiter_xml = if !delimiter.is_empty() {
        format!("    <Delimiter>{}</Delimiter>\n", xml_escape(delimiter))
    } else {
        String::new()
    };
    let encoding_xml = if url_encode {
        "    <EncodingType>url</EncodingType>\n".to_string()
    } else {
        String::new()
    };

    let truncated_str  = if truncated { "true" } else { "false" };
    // V1: botocore does NOT URL-decode top-level <Prefix>, so never url-encode it.
    // V2: botocore DOES URL-decode top-level <Prefix>, so url-encode when requested.
    let v1_prefix  = xml_escape(prefix);
    let v2_prefix  = if url_encode { s3_url_encode(prefix) } else { xml_escape(prefix) };
    let encoded_bucket = xml_escape(&bucket);

    let xml = if is_v2 {
        // Echo ContinuationToken when it was sent
        let continuation_xml = if is_continuation {
            let ct = query.get("continuation-token").unwrap();
            format!("    <ContinuationToken>{}</ContinuationToken>\n", xml_escape(ct))
        } else {
            String::new()
        };

        // NextContinuationToken when there are more results
        let next_token_xml = if truncated {
            format!("    <NextContinuationToken>{}</NextContinuationToken>\n",
                    xml_escape(&last_key))
        } else {
            String::new()
        };

        // StartAfter echoed when it was the pagination param (even alongside ContinuationToken)
        let start_after_xml = if let Some(sa) = query.get("start-after") {
            if !sa.is_empty() {
                let disp = if url_encode { s3_url_encode(sa) } else { xml_escape(sa) };
                format!("    <StartAfter>{}</StartAfter>\n", disp)
            } else { String::new() }
        } else { String::new() };

        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <ListBucketResult xmlns=\"{s3}\">\n\
                 <Name>{bucket}</Name>\n\
                 <Prefix>{prefix}</Prefix>\n\
                 {delimiter}{encoding}\
                 <MaxKeys>{max_keys}</MaxKeys>\n\
                 {continuation}\
                 {next_token}\
                 {start_after}\
                 <KeyCount>{count}</KeyCount>\n\
                 <IsTruncated>{truncated}</IsTruncated>\n\
                 {contents}{prefixes}</ListBucketResult>",
            s3          = S3_XMLNS,
            bucket      = encoded_bucket,
            prefix      = v2_prefix,
            delimiter   = delimiter_xml,
            encoding    = encoding_xml,
            max_keys    = max_keys,
            continuation = continuation_xml,
            next_token  = next_token_xml,
            start_after = start_after_xml,
            count       = count,
            truncated   = truncated_str,
            contents    = contents_xml,
            prefixes    = prefixes_xml,
        )
    } else {
        // V1: always echo <Marker>; include <NextMarker> when truncated
        let marker_val = query.get("marker").map(|s| s.as_str()).unwrap_or("");
        let disp_marker = if url_encode { s3_url_encode(marker_val) } else { xml_escape(marker_val) };

        let next_marker_xml = if truncated {
            format!("    <NextMarker>{}</NextMarker>\n", xml_escape(&last_key))
        } else {
            String::new()
        };

        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <ListBucketResult xmlns=\"{s3}\">\n\
                 <Name>{bucket}</Name>\n\
                 <Prefix>{prefix}</Prefix>\n\
                 <Marker>{marker}</Marker>\n\
                 {next_marker}\
                 {delimiter}{encoding}\
                 <MaxKeys>{max_keys}</MaxKeys>\n\
                 <IsTruncated>{truncated}</IsTruncated>\n\
                 {contents}{prefixes}</ListBucketResult>",
            s3          = S3_XMLNS,
            bucket      = encoded_bucket,
            prefix      = v1_prefix,
            marker      = disp_marker,
            next_marker = next_marker_xml,
            delimiter   = delimiter_xml,
            encoding    = encoding_xml,
            max_keys    = max_keys,
            truncated   = truncated_str,
            contents    = contents_xml,
            prefixes    = prefixes_xml,
        )
    };

    Ok(HttpResponse::Ok().content_type("application/xml").body(xml))
}

// ---------------------------------------------------------------------------
// ListObjectVersions  GET /s3/{bucket}?versions
// For non-versioned buckets each object appears as VersionId=null / IsLatest=true.
// ---------------------------------------------------------------------------

async fn s3_list_object_versions_handler_inner(bucket: &str, req: &HttpRequest) -> Result<HttpResponse, Error> {
    let auth_result = authenticate_s3_request(req).await?;
    let db = MetadataService::new(&auth_result.user_id)?;

    if let Err(resp) = require_bucket(&db, bucket) { return Ok(resp); }

    info!("S3 ListObjectVersions: bucket={}", bucket);

    let objects = db.list_objects(bucket)?;
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S.000Z");

    let mut versions_xml = String::new();
    for key in &objects {
        let (size, etag, last_modified) = db.get_object_full(bucket, key)
            .map(|m| (
                m.size,
                m.etag.unwrap_or_default(),
                m.last_modified.unwrap_or_else(|| now.to_string()),
            ))
            .unwrap_or((0, String::new(), now.to_string()));
        versions_xml.push_str(&format!(
            "    <Version>\n\
             \t<Key>{key}</Key>\n\
             \t<VersionId>null</VersionId>\n\
             \t<IsLatest>true</IsLatest>\n\
             \t<LastModified>{lm}</LastModified>\n\
             \t<ETag>&quot;{etag}&quot;</ETag>\n\
             \t<Size>{size}</Size>\n\
             \t<StorageClass>STANDARD</StorageClass>\n\
             \t<Owner><ID>admin</ID><DisplayName>admin</DisplayName></Owner>\n\
             \t</Version>\n",
            key = xml_escape(key),
            lm = last_modified,
            etag = etag.trim_matches('"'),
            size = size,
        ));
    }

    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <ListVersionsResult xmlns=\"{s3}\">\n\
             <Name>{bucket}</Name>\n\
             <Prefix></Prefix>\n\
             <KeyMarker></KeyMarker>\n\
             <VersionIdMarker></VersionIdMarker>\n\
             <MaxKeys>1000</MaxKeys>\n\
             <IsTruncated>false</IsTruncated>\n\
             {versions}\
         </ListVersionsResult>",
        s3 = S3_XMLNS, bucket = xml_escape(bucket), versions = versions_xml,
    );
    Ok(HttpResponse::Ok().content_type("application/xml").body(xml))
}

// ---------------------------------------------------------------------------
// DeleteObjects  POST /s3/{bucket}?delete
// Parses the <Delete><Object><Key>...</Key></Object>...</Delete> body and
// deletes each named key. Returns a <DeleteResult> response.
// ---------------------------------------------------------------------------

pub async fn s3_delete_objects_handler(
    path: web::Path<String>,
    query: web::Query<HashMap<String, String>>,
    mut payload: web::Payload,
    req: HttpRequest,
) -> Result<HttpResponse, Error> {
    if !query.contains_key("delete") {
        return Ok(s3_error(StatusCode::BAD_REQUEST, "InvalidRequest",
                           "Missing ?delete parameter for multi-object delete", ""));
    }
    let bucket = path.into_inner();
    let auth_result = authenticate_s3_request(&req).await?;
    let db = MetadataService::new(&auth_result.user_id)?;

    if let Err(resp) = require_bucket(&db, &bucket) { return Ok(resp); }

    let mut body_bytes: Vec<u8> = Vec::new();
    while let Some(chunk) = payload.next().await {
        let chunk = chunk.map_err(|e| {
            warn!("DeleteObjects: payload read error: {}", e);
            actix_web::error::ErrorInternalServerError("Error reading payload")
        })?;
        body_bytes.extend_from_slice(&chunk);
    }
    let body = String::from_utf8_lossy(&body_bytes);

    // Parse each <Object> block extracting Key plus optional ETag/LastModifiedTime/Size.
    struct ObjReq {
        key: String,
        etag: Option<String>,
        last_modified_time: Option<String>,
        size: Option<u64>,
    }
    let objects: Vec<ObjReq> = {
        let mut objs = Vec::new();
        let mut remaining = body.as_ref();
        while let Some(obj_start) = remaining.find("<Object>") {
            remaining = &remaining[obj_start + 8..];
            let obj_end = remaining.find("</Object>").unwrap_or(remaining.len());
            let obj_body = &remaining[..obj_end];
            if let Some(key) = extract_xml_tag(obj_body, "Key") {
                objs.push(ObjReq {
                    key,
                    etag: extract_xml_tag(obj_body, "ETag"),
                    last_modified_time: extract_xml_tag(obj_body, "LastModifiedTime"),
                    size: extract_xml_tag(obj_body, "Size").and_then(|s| s.parse::<u64>().ok()),
                });
            }
            remaining = &remaining[obj_end..];
        }
        objs
    };

    let context = crate::service::user_context::UserContext::with_bucket(
        auth_result.user_id.clone(), auth_result.bucket.clone()
    );
    let storage_service = crate::service::storage_service::StorageService::new();

    let mut deleted_xml = String::new();
    let mut errors_xml = String::new();

    for obj_req in &objects {
        let key = &obj_req.key;
        let has_cond = obj_req.etag.is_some() || obj_req.last_modified_time.is_some() || obj_req.size.is_some();

        if has_cond && db.check_key(&bucket, key)? {
            let meta = db.get_object_full(&bucket, key)?;
            let mut failed = false;

            if !failed {
                if let Some(ref req_etag) = obj_req.etag {
                    if normalize_etag(req_etag) != normalize_etag(meta.etag.as_deref().unwrap_or("")) {
                        failed = true;
                    }
                }
            }
            if !failed {
                if let Some(ref req_mtime) = obj_req.last_modified_time {
                    if req_mtime != meta.last_modified.as_deref().unwrap_or("") {
                        failed = true;
                    }
                }
            }
            if !failed {
                if let Some(req_size) = obj_req.size {
                    if req_size != meta.size {
                        failed = true;
                    }
                }
            }

            if failed {
                errors_xml.push_str(&format!(
                    "    <Error><Key>{}</Key><Code>PreconditionFailed</Code>\
                     <Message>At least one of the pre-conditions you specified did not hold</Message></Error>\n",
                    xml_escape(key),
                ));
                continue;
            }
        }

        if db.check_key(&bucket, key)? {
            storage_service.delete_object(&context, key)?;
        }
        db.delete_metadata(&bucket, key).ok();
        deleted_xml.push_str(&format!(
            "    <Deleted><Key>{}</Key></Deleted>\n",
            xml_escape(key),
        ));
    }

    info!("S3 DeleteObjects: bucket={} objects={}", bucket, objects.len());

    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <DeleteResult xmlns=\"{s3}\">\n\
             {deleted}{errors}\
         </DeleteResult>",
        s3 = S3_XMLNS, deleted = deleted_xml, errors = errors_xml,
    );
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

    // Self-copy without REPLACE directive is invalid (S3 returns 400 InvalidRequest).
    let directive_early = req.headers().get("x-amz-metadata-directive")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("COPY");
    if src_bucket == dst_bucket && src_key == dst_key && directive_early != "REPLACE" {
        return Ok(s3_error(StatusCode::BAD_REQUEST, "InvalidRequest",
                           "This copy request is illegal because it is trying to copy an object \
                            to itself without changing the object's metadata, storage class, \
                            website redirect location or encryption attributes.",
                           &format!("/{}/{}", src_bucket, src_key)));
    }

    if !db.check_key(&src_bucket, &src_key)? {
        return Ok(s3_error(StatusCode::NOT_FOUND, "NoSuchKey",
                           "The source key does not exist", &format!("/{}/{}", src_bucket, src_key)));
    }

    let src_meta = db.get_object_full(&src_bucket, &src_key)?;

    // CopyObject conditionals: x-amz-copy-source-if-match / x-amz-copy-source-if-none-match
    let copy_if_match = req.headers().get("x-amz-copy-source-if-match")
        .and_then(|v| v.to_str().ok()).map(|s| s.trim().to_string());
    let copy_if_none_match = req.headers().get("x-amz-copy-source-if-none-match")
        .and_then(|v| v.to_str().ok()).map(|s| s.trim().to_string());
    let src_etag_val = src_meta.etag.clone().unwrap_or_default();
    let copy_resource = format!("/{}/{}", src_bucket, src_key);
    if let Some(ref im) = copy_if_match {
        if im != "*" && normalize_etag(im) != normalize_etag(&src_etag_val) {
            return Ok(s3_precondition_failed(&copy_resource));
        }
    }
    if let Some(ref inm) = copy_if_none_match {
        if inm == "*" || normalize_etag(inm) == normalize_etag(&src_etag_val) {
            return Ok(s3_precondition_failed(&copy_resource));
        }
    }

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
                    let v = String::from_utf8_lossy(value.as_bytes()).trim().to_string();
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

    // Conditional CompleteMultipartUpload: If-Match / If-None-Match (checks existing object).
    let if_match_cmu = req.headers().get("if-match")
        .and_then(|v| v.to_str().ok()).map(|s| s.trim().to_string());
    let if_none_match_cmu = req.headers().get("if-none-match")
        .and_then(|v| v.to_str().ok()).map(|s| s.trim().to_string());
    if if_match_cmu.is_some() || if_none_match_cmu.is_some() {
        let resource = format!("/{}/{}", bucket, key);
        let obj_exists = db.check_key(&bucket, &key)?;
        let cur_etag = if obj_exists {
            db.get_object_full(&bucket, &key)?.etag.unwrap_or_default()
        } else {
            String::new()
        };
        if let Some(ref im) = if_match_cmu {
            if !obj_exists {
                return Ok(s3_error(StatusCode::NOT_FOUND, "NoSuchKey",
                                   "The specified key does not exist.", &resource));
            }
            if im != "*" && normalize_etag(im) != normalize_etag(&cur_etag) {
                return Ok(s3_precondition_failed(&resource));
            }
        }
        if let Some(ref inm) = if_none_match_cmu {
            if obj_exists && (inm == "*" || normalize_etag(inm) == normalize_etag(&cur_etag)) {
                return Ok(s3_precondition_failed(&resource));
            }
        }
    }

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

    // Store the final object with a proper ETag so subsequent conditional requests can
    // check against it. Use put_object_full so the etag field is persisted.
    let multipart_etag = format!("\"s3-etag-multipart-{}\"", parts.len());
    let mut final_metadata = Metadata::from_offset_size_list(final_extents.clone());
    final_metadata.etag = Some(multipart_etag.clone());
    final_metadata.last_modified = Some(rfc2616_now());
    db.put_object_full(&bucket, &key, final_metadata)?;

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
             <ETag>{etag}</ETag>\n\
         </CompleteMultipartUploadResult>",
        s3 = S3_XMLNS,
        bucket = xml_escape(&bucket),
        key = xml_escape(&key),
        etag = xml_escape(&multipart_etag),
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
