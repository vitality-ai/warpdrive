// CopyObject handler.
use actix_web::{web, HttpRequest, HttpResponse, Error, http::StatusCode};
use log::info;

use std::collections::HashMap;

use crate::metadata::Metadata;
use crate::s3::auth::authenticate_s3_request;
use crate::service::metadata_service::MetadataService;
use crate::service::storage_service::{StorageService, StorageMode};
use crate::service::user_context::UserContext;
use crate::util::serializer::deserialize_offset_size;

use super::common::*;

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

    let source_raw = copy_source.trim_start_matches('/');
    let (source_path, copy_source_version_id) = if let Some(pos) = source_raw.find("?versionId=") {
        let vid = percent_decode(&source_raw[pos + 11..]);
        (&source_raw[..pos], if vid == "null" { None } else { Some(vid) })
    } else {
        (source_raw, None)
    };
    let (src_bucket, src_key_enc) = match source_path.splitn(2, '/').collect::<Vec<_>>().as_slice() {
        [b, k] => (b.to_string(), k.to_string()),
        _ => return Ok(s3_error(StatusCode::BAD_REQUEST, "InvalidArgument",
                                "Invalid x-amz-copy-source format (expected bucket/key)", &dst_bucket)),
    };
    let src_key = percent_decode(&src_key_enc);

    info!("S3 CopyObject: {}/{} → {}/{}", src_bucket, src_key, dst_bucket, dst_key);

    let db = MetadataService::new(&auth_result.user_id)?;
    if let Err(resp) = require_bucket(&db, &src_bucket) { return Ok(resp); }
    if let Err(resp) = require_bucket(&db, &dst_bucket) { return Ok(resp); }

    let directive_early = req.headers().get("x-amz-metadata-directive")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("COPY");
    if src_bucket == dst_bucket && src_key == dst_key && directive_early != "REPLACE" && copy_source_version_id.is_none() {
        return Ok(s3_error(StatusCode::BAD_REQUEST, "InvalidRequest",
                           "This copy request is illegal because it is trying to copy an object \
                            to itself without changing the object's metadata, storage class, \
                            website redirect location or encryption attributes.",
                           &format!("/{}/{}", src_bucket, src_key)));
    }

    let src_meta = if let Some(ref vid) = copy_source_version_id {
        match db.get_object_version(&src_bucket, &src_key, vid) {
            Ok(m) if !m.is_delete_marker => m,
            _ => return Ok(s3_error(StatusCode::NOT_FOUND, "NoSuchKey",
                                    "The source key does not exist",
                                    &format!("/{}/{}", src_bucket, src_key))),
        }
    } else {
        if !db.check_key(&src_bucket, &src_key)? {
            return Ok(s3_error(StatusCode::NOT_FOUND, "NoSuchKey",
                               "The source key does not exist", &format!("/{}/{}", src_bucket, src_key)));
        }
        db.get_object_full(&src_bucket, &src_key)?
    };

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

    let storage_service = StorageService::new();
    let src_data = storage_service.read_object(&src_context, &src_meta.to_offset_size_list(), StorageMode::S3)?;
    let new_offset_size_list = storage_service.write_object(&dst_context, &src_data, StorageMode::S3)?;

    let directive = req.headers().get("x-amz-metadata-directive")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("COPY");

    let (content_type, content_encoding, user_metadata) = if directive == "REPLACE" {
        let ct = req.headers().get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_string();
        let ce = req.headers().get("content-encoding")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
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
        (ct, ce, um)
    } else {
        (
            src_meta.content_type.clone().unwrap_or_else(|| "application/octet-stream".into()),
            src_meta.content_encoding.clone(),
            src_meta.user_metadata.clone(),
        )
    };

    let etag = md5_etag(&src_data);
    let last_modified = rfc2616_now();

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
    dst_meta.content_encoding = content_encoding;
    dst_meta.last_modified = Some(last_modified.clone());
    dst_meta.user_metadata = user_metadata;

    let (copy_vid, copy_old_extents) = db.put_object_full(&dst_bucket, &dst_key, dst_meta)?;
    if !copy_old_extents.is_empty() {
        db.queue_deletion(&dst_bucket, &dst_key, &copy_old_extents).ok();
    }

    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <CopyObjectResult xmlns=\"{s3}\">\n\
             <ETag>{etag}</ETag>\n\
             <LastModified>{lm}</LastModified>\n\
         </CopyObjectResult>",
        s3 = S3_XMLNS, etag = xml_escape(&etag), lm = last_modified,
    );
    let mut resp = HttpResponse::Ok();
    resp.content_type("application/xml");
    if let Some(vid) = copy_vid { if vid != "null" { resp.insert_header(("x-amz-version-id", vid)); } }
    if let Some(ref svid) = copy_source_version_id {
        resp.insert_header(("x-amz-copy-source-version-id", svid.clone()));
    } else if let Some(ref svid) = src_meta.version_id {
        resp.insert_header(("x-amz-copy-source-version-id", svid.clone()));
    }
    Ok(resp.body(xml))
}
