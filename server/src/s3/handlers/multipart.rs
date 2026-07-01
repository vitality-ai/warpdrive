// All multipart handlers + GetObjectAttributes + GetPart + HeadPart + complete_multipart_xml_response.
use actix_web::{web, HttpRequest, HttpResponse, Error, http::StatusCode};
use bytes::Bytes;
use futures::stream::{self, StreamExt as _};
use log::{info, warn};

use std::collections::HashMap;
use std::sync::Arc;

use serde_json;

use crate::s3::auth::authenticate_s3_request;
use crate::service::metadata_service::MetadataService;
use crate::service::storage_service::{StorageService, StorageMode};
use crate::service::user_context::UserContext;
use crate::storage::config::StorageConfig;
use crate::util::serializer::deserialize_offset_size;
use crate::metadata::Metadata;

use super::checksum::{ChecksumAlgorithm, compute_composite_checksum, verify_checksum};
use super::common::*;
use super::tagging::parse_url_tags;

// ---------------------------------------------------------------------------
// Multipart types
// ---------------------------------------------------------------------------

/// Parts manifest entry — stored as JSON in the parts_manifest column.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub(super) struct PartEntry {
    pub(super) n: i32,
    pub(super) sz: u64,
    pub(super) ext: Vec<[u64; 2]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) cksum: Option<String>,
}

/// Parse `<Part><PartNumber>N</PartNumber><ETag>e</ETag></Part>` blocks.
pub(super) fn parse_complete_multipart_xml(body: &str) -> Vec<(i32, String)> {
    let mut parts = Vec::new();
    let mut rest = body;
    while let Some(idx) = rest.find("<Part>") {
        rest = &rest[idx + 6..];
        let part_num = extract_xml_tag(rest, "PartNumber").and_then(|s| s.trim().parse::<i32>().ok());
        let etag = extract_xml_tag(rest, "ETag").map(|s| s.trim().to_string());
        if let (Some(n), Some(e)) = (part_num, etag) {
            parts.push((n, e));
        }
    }
    parts
}

// ---------------------------------------------------------------------------
// CreateMultipartUpload  POST /s3/{bucket}/{key}?uploads
// ---------------------------------------------------------------------------

pub async fn s3_create_multipart_upload_handler(
    path: web::Path<(String, String)>,
    query: web::Query<HashMap<String, String>>,
    req: HttpRequest,
) -> Result<HttpResponse, Error> {
    let (bucket, key) = path.into_inner();
    if !query.contains_key("uploads") {
        return Ok(s3_error(StatusCode::BAD_REQUEST, "InvalidRequest",
                           "Invalid multipart upload initiation request", &bucket));
    }

    let auth_result = authenticate_s3_request(&req).await?;
    let db = MetadataService::new(&auth_result.user_id)?;
    if let Err(resp) = require_bucket(&db, &bucket) { return Ok(resp); }

    let content_type = req.headers().get("content-type")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    let user_metadata: HashMap<String, String> = req.headers().iter()
        .filter_map(|(name, value)| {
            let n = name.as_str().to_lowercase();
            if n.starts_with("x-amz-meta-") {
                let k = n.trim_start_matches("x-amz-meta-").to_string();
                let v = String::from_utf8_lossy(value.as_bytes()).trim().to_string();
                Some((k, v))
            } else { None }
        })
        .collect();
    let metadata_json = serde_json::to_string(&user_metadata).unwrap_or_else(|_| "{}".to_string());

    let upload_id = format!("mpu-{}", chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0));
    let initiated_at = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S.000Z").to_string();

    // Parse checksum algorithm and type from request headers
    // boto3 sends x-amz-sdk-checksum-algorithm; raw clients use x-amz-checksum-algorithm
    let mpu_checksum_algo = req.headers().get("x-amz-sdk-checksum-algorithm")
        .or_else(|| req.headers().get("x-amz-checksum-algorithm"))
        .and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let mpu_checksum_type = if !mpu_checksum_algo.is_empty() {
        let provided_type = req.headers().get("x-amz-checksum-type")
            .and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        if provided_type.is_empty() {
            ChecksumAlgorithm::from_str(&mpu_checksum_algo)
                .map(|a| a.default_type().to_string())
                .unwrap_or_default()
        } else {
            provided_type
        }
    } else {
        String::new()
    };

    let mpu_lock_mode = req.headers().get("x-amz-object-lock-mode")
        .and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let mpu_lock_until = req.headers().get("x-amz-object-lock-retain-until-date")
        .and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let mpu_legal_hold = req.headers().get("x-amz-object-lock-legal-hold")
        .and_then(|v| v.to_str().ok()).unwrap_or("").to_string();

    db.create_multipart_upload(
        &upload_id, &bucket, &key,
        content_type.as_deref(), &metadata_json, &initiated_at,
        &mpu_checksum_algo, &mpu_checksum_type,
        &mpu_lock_mode, &mpu_lock_until, &mpu_legal_hold,
    )?;

    if let Some(tagging_str) = req.headers().get("x-amz-tagging").and_then(|v| v.to_str().ok()) {
        db.set_multipart_tagging(&upload_id, tagging_str)?;
    }

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
        uid = xml_escape(&upload_id),
    );
    let mut mpu_resp = HttpResponse::Ok();
    mpu_resp.content_type("application/xml");
    // Echo checksum algorithm in response header
    if !mpu_checksum_algo.is_empty() {
        mpu_resp.insert_header(("x-amz-checksum-algorithm", mpu_checksum_algo.clone()));
    }
    Ok(mpu_resp.body(xml))
}

// ---------------------------------------------------------------------------
// UploadPart  PUT /s3/{bucket}/{key}?partNumber=N&uploadId=ID
// ---------------------------------------------------------------------------

pub async fn s3_upload_part_handler(
    path: web::Path<(String, String)>,
    query: web::Query<HashMap<String, String>>,
    mut payload: web::Payload,
    req: HttpRequest,
) -> Result<HttpResponse, Error> {
    let (bucket, key) = path.into_inner();
    let part_number_str = query.get("partNumber")
        .ok_or_else(|| actix_web::error::ErrorBadRequest("Missing partNumber"))?.clone();
    let upload_id = query.get("uploadId")
        .ok_or_else(|| actix_web::error::ErrorBadRequest("Missing uploadId"))?.clone();

    let part_number: i32 = part_number_str.parse()
        .map_err(|_| actix_web::error::ErrorBadRequest("Invalid partNumber"))?;

    let auth_result = authenticate_s3_request(&req).await?;
    let db = MetadataService::new(&auth_result.user_id)?;

    match db.get_multipart_upload(&upload_id)? {
        Some(row) if row.status == "in_progress" => {}
        _ => return Ok(s3_error(StatusCode::NOT_FOUND, "NoSuchUpload",
                                "The specified upload does not exist", &format!("/{}/{}", bucket, key))),
    }

    let mut body: Vec<u8> = Vec::new();
    while let Some(chunk) = payload.next().await {
        let chunk = chunk.map_err(|e| {
            warn!("UploadPart: read error: {}", e);
            actix_web::error::ErrorInternalServerError("Error reading payload")
        })?;
        body.extend_from_slice(&chunk);
    }

    let context = UserContext::with_bucket(auth_result.user_id.clone(), auth_result.bucket.clone());
    let storage_service = StorageService::new();
    let offset_size_list = storage_service.write_object(&context, &body, StorageMode::S3)?;
    let extents_blob = crate::util::serializer::serialize_offset_size(&offset_size_list)?;

    let etag = format!("\"{}\"", hex::encode(md5::compute(&body).0));

    // Parse and verify per-part checksum
    let part_checksum_algo = req.headers().get("x-amz-sdk-checksum-algorithm")
        .or_else(|| req.headers().get("x-amz-checksum-algorithm"))
        .and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let part_checksum_value = if !part_checksum_algo.is_empty() {
        if let Some(algo) = ChecksumAlgorithm::from_str(&part_checksum_algo) {
            let header_name = format!("x-amz-checksum-{}", algo.header_suffix());
            if let Some(client_value) = req.headers().get(header_name.as_str())
                .and_then(|v| v.to_str().ok())
            {
                if !verify_checksum(&algo, &body, client_value) {
                    return Ok(s3_error(StatusCode::BAD_REQUEST, "BadDigest",
                        "The Content-MD5 or checksum you specified did not match what we received.",
                        &format!("/{}/{}", bucket, key)));
                }
                client_value.to_string()
            } else {
                String::new()
            }
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    db.upsert_multipart_part(&upload_id, part_number, &etag, body.len() as u64, &extents_blob, &part_checksum_value)?;

    info!("S3 UploadPart: bucket={} key={} part={} size={}", bucket, key, part_number, body.len());
    let mut part_resp = HttpResponse::Ok();
    part_resp.insert_header(("ETag", etag));
    // Echo per-part checksum in response
    if !part_checksum_value.is_empty() {
        if let Some(algo) = ChecksumAlgorithm::from_str(&part_checksum_algo) {
            let header_name = format!("x-amz-checksum-{}", algo.header_suffix());
            part_resp.insert_header((header_name, part_checksum_value));
        }
    }
    Ok(part_resp.body(""))
}

// ---------------------------------------------------------------------------
// UploadPartCopy  PUT /s3/{bucket}/{key}?partNumber=N&uploadId=ID + x-amz-copy-source
// ---------------------------------------------------------------------------

pub async fn s3_upload_part_copy_handler(
    path: web::Path<(String, String)>,
    query: web::Query<HashMap<String, String>>,
    req: HttpRequest,
) -> Result<HttpResponse, Error> {
    let (bucket, key) = path.into_inner();
    let part_number = query.get("partNumber")
        .ok_or_else(|| actix_web::error::ErrorBadRequest("Missing partNumber"))?.clone();
    let upload_id = query.get("uploadId")
        .ok_or_else(|| actix_web::error::ErrorBadRequest("Missing uploadId"))?.clone();

    let auth_result = authenticate_s3_request(&req).await?;
    let db = MetadataService::new(&auth_result.user_id)?;

    let copy_source = match req.headers().get("x-amz-copy-source") {
        Some(h) => h.to_str().unwrap_or("").to_string(),
        None => return Ok(s3_error(StatusCode::BAD_REQUEST, "InvalidArgument",
                                   "Missing x-amz-copy-source", &bucket)),
    };
    let source = copy_source.trim_start_matches('/');
    let (src_bucket, src_key_enc) = match source.splitn(2, '/').collect::<Vec<_>>().as_slice() {
        [b, k] => (b.to_string(), k.to_string()),
        _ => return Ok(s3_error(StatusCode::BAD_REQUEST, "InvalidArgument",
                                "Invalid x-amz-copy-source", &bucket)),
    };
    let src_key = percent_decode(&src_key_enc);

    if !db.check_key(&src_bucket, &src_key)? {
        return Ok(s3_error(StatusCode::NOT_FOUND, "NoSuchKey",
                           "The source key does not exist", &format!("/{}/{}", src_bucket, src_key)));
    }
    let src_meta = db.get_object_full(&src_bucket, &src_key)?;
    let src_size = src_meta.size;
    let src_extents = src_meta.to_offset_size_list();

    let copy_range_header = req.headers().get("x-amz-copy-source-range")
        .and_then(|v| v.to_str().ok()).map(|s| s.to_string());

    let (read_extents, part_size) = if let Some(ref range_str) = copy_range_header {
        let bytes_part = match range_str.strip_prefix("bytes=") {
            Some(b) => b,
            None => return Ok(s3_error(StatusCode::BAD_REQUEST, "InvalidArgument",
                                       "The x-amz-copy-source-range value is not valid", &bucket)),
        };
        let (start_s, end_s) = match bytes_part.split_once('-') {
            Some(pair) => pair,
            None => return Ok(s3_error(StatusCode::BAD_REQUEST, "InvalidArgument",
                                       "The x-amz-copy-source-range value is not valid", &bucket)),
        };
        let start: u64 = match start_s.parse() {
            Ok(n) => n,
            Err(_) => return Ok(s3_error(StatusCode::BAD_REQUEST, "InvalidArgument",
                                         "The x-amz-copy-source-range value is not valid", &bucket)),
        };
        let end: u64 = match end_s.parse() {
            Ok(n) => n,
            Err(_) => return Ok(s3_error(StatusCode::BAD_REQUEST, "InvalidArgument",
                                         "The x-amz-copy-source-range value is not valid", &bucket)),
        };
        if src_size == 0 || start >= src_size || start > end || end >= src_size {
            return Ok(s3_error(StatusCode::RANGE_NOT_SATISFIABLE, "InvalidRange",
                               "The x-amz-copy-source-range value is not valid", &bucket));
        }
        (range_slices(&src_extents, start, end), end - start + 1)
    } else {
        (range_slices(&src_extents, 0, src_size.saturating_sub(1)), src_size)
    };

    let storage_service = StorageService::new();
    let src_context = UserContext::with_bucket(auth_result.user_id.clone(), src_bucket.clone());
    let part_bytes = storage_service.read_object(&src_context, &read_extents, StorageMode::S3)?;

    let dst_context = UserContext::with_bucket(auth_result.user_id.clone(), auth_result.bucket.clone());
    let offset_size_list = storage_service.write_object(&dst_context, &part_bytes, StorageMode::S3)?;

    let part_number_i32: i32 = part_number.parse()
        .map_err(|_| actix_web::error::ErrorBadRequest("Invalid partNumber"))?;

    match db.get_multipart_upload(&upload_id)? {
        Some(row) if row.status == "in_progress" => {}
        _ => return Ok(s3_error(StatusCode::NOT_FOUND, "NoSuchUpload",
                                "The specified upload does not exist", &format!("/{}/{}", bucket, key))),
    }

    let extents_blob = crate::util::serializer::serialize_offset_size(&offset_size_list)?;
    let etag = format!("\"{}\"", hex::encode(md5::compute(&part_bytes).0));
    db.upsert_multipart_part(&upload_id, part_number_i32, &etag, part_size, &extents_blob, "")?;

    let last_modified = last_modified_now();
    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <CopyPartResult xmlns=\"{s3}\">\n\
             <LastModified>{lm}</LastModified>\n\
             <ETag>{etag}</ETag>\n\
         </CopyPartResult>",
        s3 = S3_XMLNS,
        lm = xml_escape(&last_modified),
        etag = xml_escape(&etag),
    );
    Ok(HttpResponse::Ok().content_type("application/xml").body(xml))
}

// ---------------------------------------------------------------------------
// CompleteMultipartUpload  POST /s3/{bucket}/{key}?uploadId=ID
// ---------------------------------------------------------------------------

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

    let mut body_bytes: Vec<u8> = Vec::new();
    while let Some(chunk) = payload.next().await {
        let chunk = chunk.map_err(|e| {
            warn!("CompleteMultipartUpload: read error: {}", e);
            actix_web::error::ErrorInternalServerError("Error reading payload")
        })?;
        body_bytes.extend_from_slice(&chunk);
    }
    let body_str = String::from_utf8_lossy(&body_bytes);

    let raw_parts = parse_complete_multipart_xml(&body_str);
    if raw_parts.is_empty() {
        return Ok(s3_error(StatusCode::BAD_REQUEST, "MalformedXML",
                           "The XML you provided was not well-formed or did not validate", &bucket));
    }
    let mut dedup_map: HashMap<i32, String> = HashMap::new();
    for (n, e) in &raw_parts { dedup_map.insert(*n, e.clone()); }
    let mut requested_parts: Vec<(i32, String)> = dedup_map.into_iter().collect();
    requested_parts.sort_by_key(|(n, _)| *n);
    if requested_parts.is_empty() {
        return Ok(s3_error(StatusCode::BAD_REQUEST, "MalformedXML",
                           "The XML you provided was not well-formed or did not validate", &bucket));
    }

    let upload_row = match db.get_multipart_upload(&upload_id)? {
        Some(row) if row.status == "completed" => {
            let etag = row.final_etag.unwrap_or_default();
            // For idempotent re-completion, look up stored checksum from current object metadata
            let (idem_algo, idem_val, idem_type) = if let Ok(obj_meta) = db.get_object_full(&bucket, &key) {
                (obj_meta.checksum_algorithm, obj_meta.checksum_value, obj_meta.checksum_type)
            } else {
                (None, None, None)
            };
            let idem_resp = complete_multipart_xml_response(
                &bucket, &key, &etag,
                idem_algo.as_deref(), idem_val.as_deref(), idem_type.as_deref(),
            );
            return Ok(idem_resp);
        }
        Some(row) if row.status == "in_progress" => row,
        Some(_) | None => {
            return Ok(s3_error(StatusCode::NOT_FOUND, "NoSuchUpload",
                               "The specified upload does not exist", &format!("/{}/{}", bucket, key)));
        }
    };

    let stored_parts = db.list_multipart_parts(&upload_id)?;
    let stored_map: HashMap<i32, crate::metadata::sqlite_store::MultipartPartRow> =
        stored_parts.into_iter().map(|p| (p.part_number, p)).collect();

    for (part_num, xml_etag) in &requested_parts {
        let stored = match stored_map.get(part_num) {
            Some(p) => p,
            None => return Ok(s3_error(StatusCode::BAD_REQUEST, "InvalidPart",
                                       "One or more of the specified parts could not be found",
                                       &format!("/{}/{}", bucket, key))),
        };
        if normalize_etag(xml_etag) != normalize_etag(&stored.etag) {
            return Ok(s3_error(StatusCode::BAD_REQUEST, "InvalidPart",
                               "One or more of the specified parts could not be found",
                               &format!("/{}/{}", bucket, key)));
        }
    }

    const MIN_PART_SIZE: u64 = 5 * 1024 * 1024;
    let total_parts = requested_parts.len();
    for (i, (part_num, _)) in requested_parts.iter().enumerate() {
        if i < total_parts - 1 {
            let sz = stored_map[part_num].size;
            if sz < MIN_PART_SIZE {
                return Ok(s3_error(StatusCode::BAD_REQUEST, "EntityTooSmall",
                                   "Your proposed upload is smaller than the minimum allowed object size",
                                   &format!("/{}/{}", bucket, key)));
            }
        }
    }

    let mut final_extents: Vec<(u64, u64)> = Vec::new();
    let mut manifest: Vec<PartEntry> = Vec::new();
    for (part_num, _) in &requested_parts {
        let p = &stored_map[part_num];
        let exts = deserialize_offset_size(&p.extents_blob)?;
        let ext_arr: Vec<[u64; 2]> = exts.iter().map(|&(o, s)| [o, s]).collect();
        final_extents.extend_from_slice(&exts);
        let part_cksum = if p.checksum_value.is_empty() { None } else { Some(p.checksum_value.clone()) };
        manifest.push(PartEntry { n: *part_num, sz: p.size, ext: ext_arr, cksum: part_cksum });
    }

    let mut combined_md5_bytes: Vec<u8> = Vec::new();
    for (part_num, _) in &requested_parts {
        let etag_hex = normalize_etag(&stored_map[part_num].etag).to_string();
        if let Ok(raw) = hex::decode(&etag_hex) {
            combined_md5_bytes.extend_from_slice(&raw);
        }
    }
    let multipart_etag = format!("\"{}-{}\"",
        hex::encode(md5::compute(&combined_md5_bytes).0),
        total_parts);

    let content_type = upload_row.content_type.or_else(|| Some("binary/octet-stream".to_string()));
    let user_metadata: HashMap<String, String> =
        serde_json::from_str(&upload_row.metadata_json).unwrap_or_default();

    // Compute or validate composite checksum
    let stored_algo_str = upload_row.checksum_algorithm.clone();
    let stored_type = upload_row.checksum_type.clone();
    let final_checksum_value: Option<String>;
    let final_checksum_algo: Option<String>;
    let final_checksum_type: Option<String>;

    if !stored_algo_str.is_empty() {
        if let Some(algo) = ChecksumAlgorithm::from_str(&stored_algo_str) {
            // Get client-provided composite checksum from request header
            let cksum_header = format!("x-amz-checksum-{}", algo.header_suffix());
            let client_provided = req.headers().get(cksum_header.as_str())
                .and_then(|v| v.to_str().ok()).map(|s| s.to_string());

            let computed_value = if stored_type == "FULL_OBJECT" {
                // Trust client-provided FULL_OBJECT checksum, no recomputation
                client_provided.clone()
            } else {
                // COMPOSITE: compute from stored per-part checksums
                let part_checksums: Vec<String> = requested_parts.iter()
                    .map(|(n, _)| stored_map.get(n).map(|p| p.checksum_value.clone()).unwrap_or_default())
                    .collect();
                let computed = compute_composite_checksum(&algo, &part_checksums);

                // If client provided a composite, validate it
                if let Some(ref provided) = client_provided {
                    if provided != &computed {
                        return Ok(s3_error(StatusCode::BAD_REQUEST, "BadDigest",
                            "The Content-MD5 or checksum you specified did not match what we received.",
                            &format!("/{}/{}", bucket, key)));
                    }
                }
                Some(computed)
            };

            final_checksum_value = computed_value;
            final_checksum_algo = Some(algo.as_str().to_string());
            final_checksum_type = if stored_type.is_empty() { None } else { Some(stored_type.clone()) };
        } else {
            final_checksum_value = None;
            final_checksum_algo = None;
            final_checksum_type = None;
        }
    } else {
        final_checksum_value = None;
        final_checksum_algo = None;
        final_checksum_type = None;
    }

    let total_size: u64 = final_extents.iter().map(|(_, s)| s).sum();
    let mut final_metadata = Metadata::from_offset_size_list(final_extents);
    final_metadata.etag = Some(multipart_etag.clone());
    final_metadata.size = total_size;
    final_metadata.content_type = content_type;
    final_metadata.last_modified = Some(last_modified_now());
    final_metadata.user_metadata = user_metadata;
    final_metadata.checksum_algorithm = final_checksum_algo.clone();
    final_metadata.checksum_value = final_checksum_value.clone();
    final_metadata.checksum_type = final_checksum_type.clone();
    let (mpu_vid, mpu_old_extents) = db.put_object_full(&bucket, &key, final_metadata)?;
    if !mpu_old_extents.is_empty() {
        db.queue_deletion(&bucket, &key, &mpu_old_extents).ok();
    }

    let manifest_json = serde_json::to_string(&manifest).unwrap_or_else(|_| "[]".to_string());
    db.set_parts_manifest(&bucket, &key, &manifest_json)?;
    db.mark_multipart_completed(&upload_id, &multipart_etag)?;
    db.delete_parts_for_upload(&upload_id)?;

    let tagging_str = db.get_multipart_tagging(&upload_id).unwrap_or_default();
    if !tagging_str.is_empty() {
        let tags = parse_url_tags(&tagging_str);
        let _ = db.set_object_tags(&bucket, &key, &tags);
    }

    // Apply object lock from multipart upload metadata
    let lock_mode = &upload_row.object_lock_mode;
    let lock_until = &upload_row.object_lock_retain_until;
    let legal_hold = &upload_row.object_lock_legal_hold;
    if !lock_mode.is_empty() || !legal_hold.is_empty() {
        let effective_vid = mpu_vid.as_deref().unwrap_or("");
        let hold_opt = if legal_hold.is_empty() { None } else { Some(legal_hold.as_str()) };
        let _ = db.put_object_lock(
            &bucket, &key, effective_vid,
            if lock_mode.is_empty() { None } else { Some(lock_mode.as_str()) },
            if lock_until.is_empty() { None } else { Some(lock_until.as_str()) },
            hold_opt,
        );
    }

    info!("S3 CompleteMultipartUpload: bucket={} key={} parts={} etag={}", bucket, key, total_parts, multipart_etag);
    let mut resp = complete_multipart_xml_response(
        &bucket, &key, &multipart_etag,
        final_checksum_algo.as_deref(), final_checksum_value.as_deref(), final_checksum_type.as_deref(),
    );
    let resp_vid = mpu_vid.as_deref().unwrap_or("");
    if !resp_vid.is_empty() && resp_vid != "null" {
        resp.headers_mut().insert(
            actix_web::http::header::HeaderName::from_static("x-amz-version-id"),
            actix_web::http::header::HeaderValue::from_str(resp_vid).unwrap(),
        );
    }
    Ok(resp)
}

pub(super) fn complete_multipart_xml_response(
    bucket: &str, key: &str, etag: &str,
    checksum_algo: Option<&str>, checksum_value: Option<&str>, checksum_type: Option<&str>,
) -> HttpResponse {
    // Checksum elements go inside the XML body for CompleteMultipartUpload
    let checksum_xml = match (checksum_algo, checksum_value) {
        (Some(algo_str), Some(val)) => {
            if let Some(algo) = ChecksumAlgorithm::from_str(algo_str) {
                let elem = algo.response_key();
                let type_xml = checksum_type
                    .filter(|t| !t.is_empty())
                    .map(|t| format!("    <ChecksumType>{}</ChecksumType>\n", xml_escape(t)))
                    .unwrap_or_default();
                format!("    <{elem}>{val}</{elem}>\n{type_xml}", elem = elem, val = xml_escape(val))
            } else {
                String::new()
            }
        }
        _ => String::new(),
    };
    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <CompleteMultipartUploadResult xmlns=\"{s3}\">\n\
             <Location>http://{bucket}.s3.amazonaws.com/{key}</Location>\n\
             <Bucket>{bucket}</Bucket>\n\
             <Key>{key}</Key>\n\
             <ETag>{etag}</ETag>\n\
         {cksum}</CompleteMultipartUploadResult>",
        s3 = S3_XMLNS,
        bucket = xml_escape(bucket),
        key = xml_escape(key),
        etag = xml_escape(etag),
        cksum = checksum_xml,
    );
    HttpResponse::Ok().content_type("application/xml").body(xml)
}

// ---------------------------------------------------------------------------
// AbortMultipartUpload  DELETE /s3/{bucket}/{key}?uploadId=ID
// ---------------------------------------------------------------------------

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

    match db.get_multipart_upload(&upload_id)? {
        Some(row) if row.status == "in_progress" => {}
        _ => return Ok(s3_error(StatusCode::NOT_FOUND, "NoSuchUpload",
                                "The specified upload does not exist",
                                &format!("/{}/{}", bucket, key))),
    }

    let parts = db.list_multipart_parts(&upload_id)?;
    for part in &parts {
        let extents = deserialize_offset_size(&part.extents_blob)?;
        db.queue_deletion(&bucket, &key, &extents)?;
    }
    db.delete_parts_for_upload(&upload_id)?;
    db.delete_multipart_upload(&upload_id)?;

    info!("S3 AbortMultipartUpload: bucket={} key={} uploadId={}", bucket, key, upload_id);
    Ok(HttpResponse::NoContent()
        .insert_header(("Content-Length", "0"))
        .body(""))
}

// ---------------------------------------------------------------------------
// MultipartRouter  POST /s3/{bucket}/{key}
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// ListMultipartUploads  GET /s3/{bucket}?uploads
// ---------------------------------------------------------------------------

pub(super) async fn s3_list_multipart_uploads_handler(bucket: &str, req: &HttpRequest) -> Result<HttpResponse, Error> {
    let auth_result = authenticate_s3_request(req).await?;
    let db = MetadataService::new(&auth_result.user_id)?;
    if let Err(resp) = require_bucket(&db, bucket) { return Ok(resp); }

    let uploads = db.list_multipart_uploads_for_bucket(bucket)?;

    let mut uploads_xml = String::new();
    for upload in &uploads {
        let uid = xml_escape(&upload.user_id);
        uploads_xml.push_str(&format!(
            "<Upload>\
              <Key>{key}</Key>\
              <UploadId>{upid}</UploadId>\
              <Initiator><ID>{uid}</ID><DisplayName>{uid}</DisplayName></Initiator>\
              <Owner><ID>{uid}</ID><DisplayName>{uid}</DisplayName></Owner>\
              <StorageClass>STANDARD</StorageClass>\
              <Initiated>{init}</Initiated>\
            </Upload>",
            key   = xml_escape(&upload.key),
            upid  = xml_escape(&upload.upload_id),
            uid   = uid,
            init  = xml_escape(&upload.initiated_at),
        ));
    }

    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <ListMultipartUploadsResult xmlns=\"{s3}\">\
           <Bucket>{bucket}</Bucket>\
           <KeyMarker></KeyMarker>\
           <UploadIdMarker></UploadIdMarker>\
           <NextKeyMarker></NextKeyMarker>\
           <NextUploadIdMarker></NextUploadIdMarker>\
           <MaxUploads>1000</MaxUploads>\
           <IsTruncated>false</IsTruncated>\
           {uploads}\
         </ListMultipartUploadsResult>",
        s3 = S3_XMLNS, bucket = xml_escape(bucket), uploads = uploads_xml,
    );
    Ok(HttpResponse::Ok().content_type("application/xml").body(xml))
}

// ---------------------------------------------------------------------------
// GetObjectAttributes  GET /s3/{bucket}/{key}?attributes
// ---------------------------------------------------------------------------

pub(super) async fn s3_get_object_attributes_handler(bucket: &str, key: &str, req: &HttpRequest) -> Result<HttpResponse, Error> {
    let auth_result = authenticate_s3_request(req).await?;
    let db = MetadataService::new(&auth_result.user_id)?;
    if let Err(resp) = require_bucket(&db, bucket) { return Ok(resp); }

    if !db.check_key(bucket, key)? {
        return Ok(s3_error(StatusCode::NOT_FOUND, "NoSuchKey",
                           "The specified key does not exist", &format!("/{}/{}", bucket, key)));
    }
    let meta = db.get_object_full(bucket, key)?;
    let etag_raw = meta.etag.as_deref().map(normalize_etag).unwrap_or("").to_string();

    let max_parts: usize = req.headers().get("x-amz-max-parts")
        .and_then(|v| v.to_str().ok()).and_then(|s| s.parse().ok()).unwrap_or(1000);
    let part_number_marker: i32 = req.headers().get("x-amz-part-number-marker")
        .and_then(|v| v.to_str().ok()).and_then(|s| s.parse().ok()).unwrap_or(0);

    // Pre-compute checksum algo for parts XML building
    let checksum_algo_for_parts = meta.checksum_algorithm.as_deref()
        .and_then(|s| ChecksumAlgorithm::from_str(s));

    let mut object_parts_xml = String::new();
    if let Some(manifest_json) = db.get_parts_manifest(bucket, key)? {
        if let Ok(parts) = serde_json::from_str::<Vec<PartEntry>>(&manifest_json) {
            let total = parts.len();
            let eligible: Vec<&PartEntry> = parts.iter().filter(|p| p.n > part_number_marker).collect();
            let page: Vec<&PartEntry> = eligible.iter().copied().take(max_parts).collect();
            let is_truncated = eligible.len() > max_parts;
            let next_marker = page.last().map(|p| p.n).unwrap_or(0);

            let mut parts_xml = String::new();
            for p in &page {
                let part_cksum_xml = if let (Some(ref algo), Some(ref cksum)) = (&checksum_algo_for_parts, &p.cksum) {
                    format!("<{key}>{val}</{key}>", key = algo.response_key(), val = xml_escape(cksum))
                } else {
                    String::new()
                };
                parts_xml.push_str(&format!(
                    "<Part><PartNumber>{}</PartNumber><Size>{}</Size>{}</Part>",
                    p.n, p.sz, part_cksum_xml
                ));
            }

            let pn_marker_xml = if part_number_marker > 0 {
                format!("<PartNumberMarker>{}</PartNumberMarker>", part_number_marker)
            } else { String::new() };
            let max_parts_xml = if max_parts < 1000 {
                format!("<MaxParts>{}</MaxParts>", max_parts)
            } else { String::new() };
            let next_xml = if is_truncated {
                format!("<NextPartNumberMarker>{}</NextPartNumberMarker>", next_marker)
            } else { String::new() };

            object_parts_xml = format!(
                "<ObjectParts>\
                  <TotalPartsCount>{total}</TotalPartsCount>\
                  <PartsCount>{total}</PartsCount>\
                  {pnm}{maxp}<IsTruncated>{trunc}</IsTruncated>\
                  {next}{parts}\
                </ObjectParts>",
                total = total, pnm = pn_marker_xml, maxp = max_parts_xml,
                trunc = is_truncated, next = next_xml, parts = parts_xml,
            );
        }
    }

    // Build checksum XML if object has a stored checksum
    let mut checksum_xml = String::new();
    if let (Some(ref algo_str), Some(ref cksum_val)) = (&meta.checksum_algorithm, &meta.checksum_value) {
        if let Some(algo) = ChecksumAlgorithm::from_str(algo_str) {
            let cksum_type_xml = if let Some(ref ct) = meta.checksum_type {
                if !ct.is_empty() {
                    format!("<ChecksumType>{}</ChecksumType>", xml_escape(ct))
                } else {
                    String::new()
                }
            } else {
                String::new()
            };
            checksum_xml = format!(
                "<Checksum><{key}>{val}</{key}>{ct}</Checksum>",
                key = algo.response_key(),
                val = xml_escape(cksum_val),
                ct = cksum_type_xml,
            );
        }
    }

    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <GetObjectAttributesResponse xmlns=\"{s3}\">\
           <ETag>{etag}</ETag>\
           <StorageClass>STANDARD</StorageClass>\
           <ObjectSize>{sz}</ObjectSize>\
           {checksum}\
           {parts}\
         </GetObjectAttributesResponse>",
        s3 = S3_XMLNS,
        etag = xml_escape(&etag_raw),
        sz = meta.size,
        checksum = checksum_xml,
        parts = object_parts_xml,
    );
    let mut resp = HttpResponse::Ok();
    resp.content_type("application/xml");
    if let Some(lm) = &meta.last_modified { resp.insert_header(("Last-Modified", last_modified_for_header(lm))); }
    Ok(resp.body(xml))
}

// ---------------------------------------------------------------------------
// GET/HEAD ?partNumber — serve a specific part of a multipart object
// ---------------------------------------------------------------------------

pub(super) async fn s3_get_part_handler(bucket: &str, key: &str, part_num: i32, req: &HttpRequest) -> Result<HttpResponse, Error> {
    let auth_result = authenticate_s3_request(req).await?;
    let db = MetadataService::new(&auth_result.user_id)?;
    if let Err(resp) = require_bucket(&db, bucket) { return Ok(resp); }

    if !db.check_key(bucket, key)? {
        return Ok(s3_error(StatusCode::NOT_FOUND, "NoSuchKey",
                           "The specified key does not exist", &format!("/{}/{}", bucket, key)));
    }

    let meta = db.get_object_full(bucket, key)?;
    let etag = meta.etag.clone().unwrap_or_default();
    let content_type = meta.content_type.clone().unwrap_or_else(|| "application/octet-stream".into());

    if let Some(manifest_json) = db.get_parts_manifest(bucket, key)? {
        if let Ok(parts) = serde_json::from_str::<Vec<PartEntry>>(&manifest_json) {
            let total_parts = parts.len() as i32;
            let part = match parts.iter().find(|p| p.n == part_num) {
                Some(p) => p,
                None => return Ok(s3_error(StatusCode::BAD_REQUEST, "InvalidPart",
                                           "The requested partnumber is not satisfiable",
                                           &format!("/{}/{}", bucket, key))),
            };
            let extents: Vec<(u64, u64)> = part.ext.iter().map(|e| (e[0], e[1])).collect();
            let part_size = part.sz;

            let slices = Arc::new(stream_slices(&extents));
            let store = StorageConfig::from_env().create_store();
            let context = UserContext::with_bucket(auth_result.user_id.clone(), auth_result.bucket.clone());
            let byte_stream = stream::try_unfold(0usize, move |idx| {
                let slices = Arc::clone(&slices);
                let ctx = context.clone();
                let store = Arc::clone(&store);
                async move {
                    if idx >= slices.len() { return Ok::<Option<(Bytes, usize)>, Error>(None); }
                    let (off, sz) = slices[idx];
                    let chunk = web::block(move || store.read(&ctx.user_id, &ctx.bucket, off, sz).map_err(|e| e.to_string()))
                        .await.map_err(actix_web::error::ErrorInternalServerError)?
                        .map_err(actix_web::error::ErrorInternalServerError)?;
                    Ok(Some((Bytes::from(chunk), idx + 1)))
                }
            });

            let mut resp = HttpResponse::Ok();
            resp.content_type(content_type.as_str());
            resp.insert_header(("Content-Length", part_size.to_string()));
            resp.insert_header(("ETag", etag));
            resp.insert_header(("x-amz-mp-parts-count", total_parts.to_string()));
            // Return checksum headers for this part if available
            if let (Some(ref algo_str), Some(ref ct)) = (&meta.checksum_algorithm, &meta.checksum_type) {
                if !algo_str.is_empty() && !ct.is_empty() {
                    resp.insert_header(("x-amz-checksum-type", ct.clone()));
                }
            }
            if let Some(ref algo_str) = &meta.checksum_algorithm {
                if let Some(algo) = ChecksumAlgorithm::from_str(algo_str) {
                    if let Some(ref part_cksum) = part.cksum {
                        let header_name = format!("x-amz-checksum-{}", algo.header_suffix());
                        resp.insert_header((header_name, part_cksum.clone()));
                    }
                }
            }
            return Ok(resp.streaming(byte_stream));
        }
    }

    if part_num > 1 {
        return Ok(s3_error(StatusCode::BAD_REQUEST, "InvalidPart",
                           "The requested partnumber is not satisfiable",
                           &format!("/{}/{}", bucket, key)));
    }

    let total_size = meta.size;
    let extents = meta.to_offset_size_list();
    let slices = Arc::new(stream_slices(&extents));
    let store = StorageConfig::from_env().create_store();
    let context = UserContext::with_bucket(auth_result.user_id.clone(), auth_result.bucket.clone());
    let byte_stream = stream::try_unfold(0usize, move |idx| {
        let slices = Arc::clone(&slices);
        let ctx = context.clone();
        let store = Arc::clone(&store);
        async move {
            if idx >= slices.len() { return Ok::<Option<(Bytes, usize)>, Error>(None); }
            let (off, sz) = slices[idx];
            let chunk = web::block(move || store.read(&ctx.user_id, &ctx.bucket, off, sz).map_err(|e| e.to_string()))
                .await.map_err(actix_web::error::ErrorInternalServerError)?
                .map_err(actix_web::error::ErrorInternalServerError)?;
            Ok(Some((Bytes::from(chunk), idx + 1)))
        }
    });

    let mut resp = HttpResponse::Ok();
    resp.content_type(content_type.as_str());
    resp.insert_header(("Content-Length", total_size.to_string()));
    resp.insert_header(("ETag", etag));
    Ok(resp.streaming(byte_stream))
}

pub(super) async fn s3_head_part_handler(bucket: &str, key: &str, part_num: i32, req: &HttpRequest) -> Result<HttpResponse, Error> {
    let auth_result = authenticate_s3_request(req).await?;
    let db = MetadataService::new(&auth_result.user_id)?;
    if let Err(resp) = require_bucket(&db, bucket) { return Ok(resp); }

    if !db.check_key(bucket, key)? {
        return Ok(s3_error(StatusCode::NOT_FOUND, "NoSuchKey",
                           "The specified key does not exist", &format!("/{}/{}", bucket, key)));
    }

    let meta = db.get_object_full(bucket, key)?;
    let etag = meta.etag.clone().unwrap_or_default();
    let content_type = meta.content_type.clone().unwrap_or_else(|| "application/octet-stream".into());

    if let Some(manifest_json) = db.get_parts_manifest(bucket, key)? {
        if let Ok(parts) = serde_json::from_str::<Vec<PartEntry>>(&manifest_json) {
            let total_parts = parts.len() as i32;
            let part = match parts.iter().find(|p| p.n == part_num) {
                Some(p) => p,
                None => return Ok(s3_error(StatusCode::BAD_REQUEST, "InvalidPart",
                                           "The requested partnumber is not satisfiable",
                                           &format!("/{}/{}", bucket, key))),
            };
            let mut resp = HttpResponse::Ok();
            resp.insert_header(("Content-Type", content_type));
            resp.insert_header(("ETag", etag));
            resp.insert_header(("x-amz-mp-parts-count", total_parts.to_string()));
            return Ok(resp.message_body(HeadBody(part.sz)).unwrap().map_into_boxed_body());
        }
    }

    if part_num > 1 {
        return Ok(s3_error(StatusCode::BAD_REQUEST, "InvalidPart",
                           "The requested partnumber is not satisfiable",
                           &format!("/{}/{}", bucket, key)));
    }

    let mut resp = HttpResponse::Ok();
    resp.insert_header(("Content-Type", content_type));
    resp.insert_header(("ETag", etag));
    Ok(resp.message_body(HeadBody(meta.size)).unwrap().map_into_boxed_body())
}
