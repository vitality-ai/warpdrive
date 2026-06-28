// PutObject, GetObject, HeadObject, DeleteObject handlers.
use actix_web::{web, HttpRequest, HttpResponse, Error, http::StatusCode};
use bytes::Bytes;
use futures::stream::{self, StreamExt as _};
use log::{debug, error, info, warn};

use std::collections::HashMap;
use std::sync::Arc;

use crate::metadata::Metadata;
use crate::s3::auth::{authenticate_s3_request, create_authenticated_request};
use crate::service::metadata_service::MetadataService;
use crate::service::storage_service::StorageService;
use crate::service::user_context::UserContext;
use crate::storage::config::StorageConfig;

use super::checksum::{parse_checksum_headers, verify_checksum, ChecksumAlgorithm};
use super::common::*;
use super::tagging::{s3_put_object_tagging_inner, s3_get_object_tagging_inner, s3_delete_object_tagging_inner, parse_url_tags, validate_tags};
use super::versioning::{s3_get_object_version_handler, s3_delete_specific_version_handler};
use super::acl::{s3_put_acl_stub, s3_get_object_acl_stub, validate_object_key};
use super::copy::s3_copy_object_handler;
use super::multipart::{s3_upload_part_handler, s3_upload_part_copy_handler, s3_abort_multipart_upload_handler, s3_get_object_attributes_handler, s3_get_part_handler, s3_head_part_handler};
use super::object_lock::{s3_put_object_retention_inner, s3_get_object_retention_inner, s3_put_object_legal_hold_inner, s3_get_object_legal_hold_inner, compute_retain_until};

// ---------------------------------------------------------------------------
// PutObject  PUT /s3/{bucket}/{key}
// ---------------------------------------------------------------------------

pub async fn s3_put_object_handler(
    path: web::Path<(String, String)>,
    mut payload: web::Payload,
    req: HttpRequest,
) -> Result<HttpResponse, Error> {
    if let Ok(query) = web::Query::<HashMap<String, String>>::from_query(req.query_string()) {
        if req.headers().contains_key("x-amz-copy-source")
            && query.contains_key("partNumber")
            && query.contains_key("uploadId")
        {
            return s3_upload_part_copy_handler(path, query, req).await;
        }
        if query.contains_key("partNumber") && query.contains_key("uploadId") {
            return s3_upload_part_handler(path, query, payload, req).await;
        }
        if query.contains_key("tagging") {
            let (bucket, key) = path.into_inner();
            let mut body: Vec<u8> = Vec::new();
            while let Some(chunk) = payload.next().await {
                body.extend_from_slice(&chunk.map_err(actix_web::error::ErrorInternalServerError)?);
            }
            return s3_put_object_tagging_inner(&bucket, &key, &body, &req).await;
        }
        if query.contains_key("acl") {
            return s3_put_acl_stub(&req).await;
        }
        if query.contains_key("retention") || query.contains_key("legal-hold") {
            let (bucket, key) = path.into_inner();
            let mut body: Vec<u8> = Vec::new();
            while let Some(chunk) = payload.next().await {
                body.extend_from_slice(&chunk.map_err(actix_web::error::ErrorInternalServerError)?);
            }
            if query.contains_key("retention") {
                return s3_put_object_retention_inner(&bucket, &key, &body, &req).await;
            }
            return s3_put_object_legal_hold_inner(&bucket, &key, &body, &req).await;
        }
    }
    if req.headers().contains_key("x-amz-copy-source") {
        return s3_copy_object_handler(path, req).await;
    }

    let (bucket, key) = path.into_inner();
    let auth_result = authenticate_s3_request(&req).await?;
    let _authenticated_req = create_authenticated_request(&req, &auth_result);

    let db = MetadataService::new(&auth_result.user_id)?;
    if let Err(resp) = require_bucket(&db, &bucket) { return Ok(resp); }

    info!("S3 PutObject: bucket={} key={} user={}", bucket, key, auth_result.user_id);

    let context = UserContext::with_bucket(auth_result.user_id.clone(), auth_result.bucket.clone());

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

    let content_encoding = req.headers()
        .get("content-encoding")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| {
            let stripped: Vec<&str> = s.split(',')
                .map(|p| p.trim())
                .filter(|p| !p.eq_ignore_ascii_case("aws-chunked"))
                .collect();
            if stripped.is_empty() { None } else { Some(stripped.join(", ")) }
        });

    let store = StorageConfig::from_env().create_store();
    let mut offset_size_list: Vec<(u64, u64)> = Vec::new();
    let mut body_buf: Vec<u8> = Vec::new();

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

    // Checksum verification
    let checksum_result = parse_checksum_headers(&req);
    if let Some((ref algo, ref client_value)) = checksum_result {
        if !verify_checksum(algo, &body_buf, client_value) {
            let resource = format!("/{}/{}", bucket, key);
            return Ok(s3_error(StatusCode::BAD_REQUEST, "BadDigest",
                               "The Content-MD5 or checksum you specified did not match what we received.",
                               &resource));
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
    metadata.content_encoding = content_encoding;
    if let Some((ref algo, ref value)) = checksum_result {
        metadata.checksum_algorithm = Some(algo.as_str().to_string());
        metadata.checksum_value = Some(value.clone());
        // For simple (non-multipart) objects, checksum_type is not set (leave None)
    }

    let (version_id, old_extents) = db.put_object_full(&bucket, &key, metadata)?;
    if !old_extents.is_empty() {
        db.queue_deletion(&bucket, &key, &old_extents).ok();
    }

    if let Some(tagging_str) = req.headers().get("x-amz-tagging").and_then(|v| v.to_str().ok()) {
        let resource = format!("/{}/{}", bucket, key);
        let tags = parse_url_tags(tagging_str);
        if let Err(resp) = validate_tags(&tags, &resource) { return Ok(resp); }
        db.set_object_tags(&bucket, &key, &tags)?;
    }

    // Apply object lock — from per-object headers or bucket default retention
    let obj_lock_mode = req.headers().get("x-amz-object-lock-mode")
        .and_then(|v| v.to_str().ok()).map(|s| s.to_string());
    let obj_lock_until = req.headers().get("x-amz-object-lock-retain-until-date")
        .and_then(|v| v.to_str().ok()).map(|s| s.to_string());
    let obj_legal_hold = req.headers().get("x-amz-object-lock-legal-hold")
        .and_then(|v| v.to_str().ok()).map(|s| s.to_string());

    let effective_vid = version_id.as_deref().unwrap_or("");
    let lock_mode: Option<String>;
    let lock_until: Option<String>;

    if obj_lock_mode.is_some() && obj_lock_until.is_some() {
        lock_mode = obj_lock_mode;
        lock_until = obj_lock_until;
    } else if let Ok(Some((def_mode, def_days, def_years))) = db.get_object_lock_config(&bucket) {
        lock_mode = Some(def_mode);
        lock_until = Some(compute_retain_until(def_days, def_years));
    } else {
        lock_mode = None;
        lock_until = None;
    }

    if lock_mode.is_some() || obj_legal_hold.is_some() {
        let _ = db.put_object_lock(
            &bucket, &key, effective_vid,
            lock_mode.as_deref(), lock_until.as_deref(),
            obj_legal_hold.as_deref(),
        );
    }

    debug!("S3 PutObject OK: bucket={} key={} size={} etag={}", bucket, key, size, etag);
    let mut resp = HttpResponse::Ok();
    resp.insert_header(("ETag", etag));
    let resp_vid = version_id.as_deref().unwrap_or("");
    if !resp_vid.is_empty() && resp_vid != "null" {
        resp.insert_header(("x-amz-version-id", resp_vid.to_string()));
    }
    if let Some(ref m) = lock_mode { resp.insert_header(("x-amz-object-lock-mode", m.clone())); }
    if let Some(ref u) = lock_until { resp.insert_header(("x-amz-object-lock-retain-until-date", u.clone())); }
    // Echo checksum header in response
    if let Some((ref algo, ref value)) = checksum_result {
        let header_name = format!("x-amz-checksum-{}", algo.header_suffix());
        resp.insert_header((header_name, value.clone()));
    }
    Ok(resp.insert_header(("Content-Length", "0")).body(""))
}

// ---------------------------------------------------------------------------
// GetObject  GET /s3/{bucket}/{key}
// ---------------------------------------------------------------------------

pub async fn s3_get_object_handler(
    path: web::Path<(String, String)>,
    req: HttpRequest,
) -> Result<HttpResponse, Error> {
    let (bucket, key) = path.into_inner();

    let qmap: HashMap<String, String> = web::Query::<HashMap<String, String>>::from_query(req.query_string())
        .map(|q| q.into_inner()).unwrap_or_default();
    if qmap.contains_key("attributes") {
        return s3_get_object_attributes_handler(&bucket, &key, &req).await;
    }
    if qmap.contains_key("tagging") {
        return s3_get_object_tagging_inner(&bucket, &key, &req).await;
    }
    // retention/legal-hold come before versionId — boto3 sends both params together
    if qmap.contains_key("retention") {
        return s3_get_object_retention_inner(&bucket, &key, &req).await;
    }
    if qmap.contains_key("legal-hold") {
        return s3_get_object_legal_hold_inner(&bucket, &key, &req).await;
    }
    if let Some(vid) = qmap.get("versionId") {
        return s3_get_object_version_handler(&bucket, &key, vid, &req).await;
    }
    if qmap.contains_key("acl") {
        return s3_get_object_acl_stub(&bucket, &key, &req).await;
    }
    if let Some(pn_str) = qmap.get("partNumber") {
        let pn: i32 = match pn_str.parse::<i32>() {
            Ok(n) if n >= 1 => n,
            _ => return Ok(s3_error(StatusCode::BAD_REQUEST, "InvalidArgument",
                                    "Part number must be an integer between 1 and 10000",
                                    &format!("/{}/{}", bucket, key))),
        };
        return s3_get_part_handler(&bucket, &key, pn, &req).await;
    }

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

        if let Some(ref im) = get_if_match {
            if im != "*" && normalize_etag(im) != normalize_etag(&etag) {
                return Ok(s3_precondition_failed(&resource));
            }
        }
        if let Some(ref ius) = get_if_unmodified {
            if let (Some(hdr_ts), Some(obj_ts)) =
                (parse_http_date(ius), parse_http_date(&last_modified))
            {
                if obj_ts > hdr_ts {
                    return Ok(s3_precondition_failed(&resource));
                }
            }
        }
        if let Some(ref inm) = get_if_none_match {
            if inm == "*" || normalize_etag(inm) == normalize_etag(&etag) {
                let mut r = HttpResponse::NotModified();
                if !etag.is_empty() { r.insert_header(("ETag", etag.as_str())); }
                if !last_modified.is_empty() { r.insert_header(("Last-Modified", last_modified.as_str())); }
                return Ok(r.finish());
            }
        }
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

    let (slices, response_len, range_header) = match parse_range_header(&req, total_size) {
        RangeResult::Valid(rs, re) => {
            let s = range_slices(&extents, rs, re);
            let len = re - rs + 1;
            let hdr = format!("bytes {}-{}/{}", rs, re, total_size);
            (s, len, Some(hdr))
        }
        RangeResult::Unsatisfiable => {
            let resource = format!("/{}/{}", bucket, key);
            return Ok(s3_error(StatusCode::RANGE_NOT_SATISFIABLE, "InvalidRange",
                               "The requested range is not valid for the request. \
                                Please try another range.", &resource));
        }
        RangeResult::None => (stream_slices(&extents), total_size, None),
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

    let resp_content_type = qmap.get("response-content-type").cloned()
        .unwrap_or(content_type);
    let resp_content_disposition = qmap.get("response-content-disposition").cloned();
    let resp_content_language = qmap.get("response-content-language").cloned();
    let resp_expires = qmap.get("response-expires").cloned()
        .or_else(|| meta.expires.clone());
    let resp_cache_control = qmap.get("response-cache-control").cloned()
        .or_else(|| meta.cache_control.clone());
    let resp_content_encoding = qmap.get("response-content-encoding").cloned()
        .or_else(|| meta.content_encoding.clone());

    let status = if range_header.is_some() { StatusCode::PARTIAL_CONTENT } else { StatusCode::OK };
    let mut resp = HttpResponse::build(status);
    resp.content_type(resp_content_type.as_str());
    resp.insert_header(("Content-Length", response_len.to_string()));
    resp.insert_header(("ETag", etag));
    resp.insert_header(("Accept-Ranges", "bytes"));
    if let Some(cr) = range_header {
        resp.insert_header(("Content-Range", cr));
    }
    if !last_modified.is_empty() {
        resp.insert_header(("Last-Modified", last_modified));
    }
    if let Some(cc) = resp_cache_control {
        resp.insert_header(("Cache-Control", cc));
    }
    if let Some(exp) = resp_expires {
        resp.insert_header(("Expires", exp));
    }
    if let Some(enc) = resp_content_encoding {
        resp.insert_header(("Content-Encoding", enc));
    }
    if let Some(cd) = resp_content_disposition {
        resp.insert_header(("Content-Disposition", cd));
    }
    if let Some(cl) = resp_content_language {
        resp.insert_header(("Content-Language", cl));
    }
    for (k, v) in &meta.user_metadata {
        resp.insert_header((format!("x-amz-meta-{}", k), metadata_value_header(v)));
    }
    if let Some(ref vid) = meta.version_id {
        resp.insert_header(("x-amz-version-id", vid.clone()));
    }
    // Return checksum headers when x-amz-checksum-mode: ENABLED
    let checksum_mode = req.headers().get("x-amz-checksum-mode")
        .and_then(|v| v.to_str().ok()).map(|s| s.to_uppercase());
    if checksum_mode.as_deref() == Some("ENABLED") {
        if let (Some(ref algo_str), Some(ref cksum_val)) = (&meta.checksum_algorithm, &meta.checksum_value) {
            if let Some(algo) = ChecksumAlgorithm::from_str(algo_str) {
                let header_name = format!("x-amz-checksum-{}", algo.header_suffix());
                resp.insert_header((header_name, cksum_val.clone()));
                if let Some(ref cksum_type) = meta.checksum_type {
                    if !cksum_type.is_empty() {
                        resp.insert_header(("x-amz-checksum-type", cksum_type.clone()));
                    }
                }
            }
        }
    }
    // Return object lock headers if present
    if let Some(ref vid) = meta.version_id {
        if let Ok(Some(lock)) = db.get_object_lock(&bucket, &key, vid) {
            if let Some(ref m) = lock.mode { resp.insert_header(("x-amz-object-lock-mode", m.clone())); }
            if let Some(ref u) = lock.retain_until_date { resp.insert_header(("x-amz-object-lock-retain-until-date", u.clone())); }
            if lock.legal_hold == "ON" { resp.insert_header(("x-amz-object-lock-legal-hold", "ON")); }
        }
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

    let qmap: HashMap<String, String> = web::Query::<HashMap<String, String>>::from_query(req.query_string())
        .map(|q| q.into_inner()).unwrap_or_default();
    if let Some(pn_str) = qmap.get("partNumber") {
        let pn: i32 = match pn_str.parse::<i32>() {
            Ok(n) if n >= 1 => n,
            _ => return Ok(s3_error(StatusCode::BAD_REQUEST, "InvalidArgument",
                                    "Part number must be an integer between 1 and 10000",
                                    &format!("/{}/{}", bucket, key))),
        };
        return s3_head_part_handler(&bucket, &key, pn, &req).await;
    }
    if let Some(vid) = qmap.get("versionId") {
        let resp = s3_get_object_version_handler(&bucket, &key, vid, &req).await?;
        if resp.status().is_success() {
            let mut head_resp = HttpResponse::build(resp.status());
            for (name, value) in resp.headers() {
                head_resp.insert_header((name.clone(), value.clone()));
            }
            return Ok(head_resp.finish());
        }
        return Ok(resp);
    }

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
    if let Some(enc) = &meta.content_encoding {
        resp.insert_header(("Content-Encoding", enc.as_str()));
    }
    for (k, v) in &meta.user_metadata {
        resp.insert_header((format!("x-amz-meta-{}", k), metadata_value_header(v)));
    }
    if let Ok(count) = db.get_object_tag_count(&bucket, &key) {
        if count > 0 {
            resp.insert_header(("x-amz-tagging-count", count.to_string()));
        }
    }
    if let Some(ref vid) = meta.version_id {
        resp.insert_header(("x-amz-version-id", vid.clone()));
    }
    // Return checksum headers when x-amz-checksum-mode: ENABLED
    let head_checksum_mode = req.headers().get("x-amz-checksum-mode")
        .and_then(|v| v.to_str().ok()).map(|s| s.to_uppercase());
    if head_checksum_mode.as_deref() == Some("ENABLED") {
        if let (Some(ref algo_str), Some(ref cksum_val)) = (&meta.checksum_algorithm, &meta.checksum_value) {
            if let Some(algo) = ChecksumAlgorithm::from_str(algo_str) {
                let header_name = format!("x-amz-checksum-{}", algo.header_suffix());
                resp.insert_header((header_name, cksum_val.clone()));
                if let Some(ref cksum_type) = meta.checksum_type {
                    if !cksum_type.is_empty() {
                        resp.insert_header(("x-amz-checksum-type", cksum_type.clone()));
                    }
                }
            }
        }
    }
    // Return object lock headers if present
    let vid_for_lock = meta.version_id.as_deref().unwrap_or("");
    if !vid_for_lock.is_empty() {
        if let Ok(Some(lock)) = db.get_object_lock(&bucket, &key, vid_for_lock) {
            if let Some(ref m) = lock.mode { resp.insert_header(("x-amz-object-lock-mode", m.clone())); }
            if let Some(ref u) = lock.retain_until_date { resp.insert_header(("x-amz-object-lock-retain-until-date", u.clone())); }
            if lock.legal_hold == "ON" { resp.insert_header(("x-amz-object-lock-legal-hold", "ON")); }
        }
    }
    Ok(resp.message_body(HeadBody(object_size)).unwrap().map_into_boxed_body())
}

// ---------------------------------------------------------------------------
// DeleteObject  DELETE /s3/{bucket}/{key}
// ---------------------------------------------------------------------------

pub async fn s3_delete_object_handler(
    path: web::Path<(String, String)>,
    req: HttpRequest,
) -> Result<HttpResponse, Error> {
    if let Ok(query) = web::Query::<HashMap<String, String>>::from_query(req.query_string()) {
        if query.contains_key("uploadId") {
            return s3_abort_multipart_upload_handler(path, query, req).await;
        }
        if query.contains_key("tagging") {
            let (bucket, key) = path.into_inner();
            return s3_delete_object_tagging_inner(&bucket, &key, &req).await;
        }
        if let Some(vid) = query.get("versionId").cloned() {
            let (bucket, key) = path.into_inner();
            return s3_delete_specific_version_handler(&bucket, &key, &vid, &req).await;
        }
    }

    let (bucket, key) = path.into_inner();

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

    use crate::metadata::sqlite_store::VersioningDeleteResult;

    let del_result = db.delete_object_v2(&bucket, &key)?;

    info!("S3 DeleteObject: bucket={} key={}", bucket, key);

    let mut resp = HttpResponse::NoContent();
    match del_result {
        VersioningDeleteResult::Marker { version_id } => {
            resp.insert_header(("x-amz-delete-marker", "true"));
            resp.insert_header(("x-amz-version-id", version_id));
        }
        VersioningDeleteResult::Deleted => {
            if db.check_key(&bucket, &key).unwrap_or(false) {
                if let Ok(existing) = db.get_object_full(&bucket, &key) {
                    let extents = existing.to_offset_size_list();
                    if !extents.is_empty() {
                        db.queue_deletion(&bucket, &key, &extents).ok();
                    }
                }
                StorageService::new().delete_object(&context, &key).ok();
            }
            db.delete_metadata(&bucket, &key).ok();
        }
    }
    Ok(resp.insert_header(("Content-Length", "0")).body(""))
}
