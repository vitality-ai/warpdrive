// s3_list_objects_handler, s3_delete_objects_handler.
use actix_web::{web, HttpRequest, HttpResponse, Error, http::StatusCode};
use futures::StreamExt as _;
use log::{info, warn};

use std::collections::HashMap;

use crate::s3::auth::authenticate_s3_request;
use crate::service::metadata_service::MetadataService;

use super::common::*;
use super::tagging::s3_get_bucket_tagging_inner;
use super::versioning::{s3_get_bucket_versioning_inner, s3_list_object_versions_handler_inner};
use super::cors::{s3_get_bucket_cors_inner, s3_get_bucket_location_inner};
use super::acl::s3_get_bucket_acl_stub;
use super::multipart::s3_list_multipart_uploads_handler;
use super::object_lock::s3_get_bucket_object_lock_inner;

// ---------------------------------------------------------------------------
// ListObjects  GET /s3/{bucket}
// ---------------------------------------------------------------------------

pub async fn s3_list_objects_handler(
    path: web::Path<String>,
    query: web::Query<HashMap<String, String>>,
    req: HttpRequest,
) -> Result<HttpResponse, Error> {
    let bucket = path.into_inner();

    if query.contains_key("versions") {
        return s3_list_object_versions_handler_inner(&bucket, &req).await;
    }

    if query.contains_key("uploads") {
        return s3_list_multipart_uploads_handler(&bucket, &req).await;
    }

    if query.contains_key("location") {
        return s3_get_bucket_location_inner(&bucket, &req).await;
    }

    if query.contains_key("cors") {
        return s3_get_bucket_cors_inner(&bucket, &req).await;
    }

    if query.contains_key("tagging") {
        return s3_get_bucket_tagging_inner(&bucket, &req).await;
    }
    if query.contains_key("versioning") {
        return s3_get_bucket_versioning_inner(&bucket, &req).await;
    }
    if query.contains_key("object-lock") {
        return s3_get_bucket_object_lock_inner(&bucket, &req).await;
    }
    if query.contains_key("acl") {
        return s3_get_bucket_acl_stub(&bucket, &req).await;
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

    if allow_unordered && !delimiter.is_empty() {
        return Ok(s3_error(StatusCode::BAD_REQUEST, "InvalidArgument",
                           "allow-unordered is not supported with delimiter", &bucket));
    }

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

    let mut contents_xml   = String::new();
    let mut prefixes_xml   = String::new();
    let mut last_common_prefix = String::new();
    let mut last_key       = String::new();
    let mut count          = 0usize;
    let mut truncated      = false;

    if max_keys > 0 {
        'outer: for key in &all_keys {
            let key = key.as_str();

            if !effective_marker.is_empty() && key <= effective_marker {
                continue;
            }

            if !key.starts_with(prefix) {
                continue;
            }

            if !delimiter.is_empty() {
                let after_prefix = &key[prefix.len()..];
                if let Some(pos) = after_prefix.find(delimiter) {
                    let group = format!("{}{}{}", prefix, &after_prefix[..pos], delimiter);

                    if !effective_marker.is_empty() && group.as_str() <= effective_marker {
                        continue;
                    }

                    if group == last_common_prefix {
                        continue;
                    }

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
    let v1_prefix  = xml_escape(prefix);
    let v2_prefix  = if url_encode { s3_url_encode(prefix) } else { xml_escape(prefix) };
    let encoded_bucket = xml_escape(&bucket);

    let xml = if is_v2 {
        let continuation_xml = if is_continuation {
            let ct = query.get("continuation-token").unwrap();
            format!("    <ContinuationToken>{}</ContinuationToken>\n", xml_escape(ct))
        } else {
            String::new()
        };

        let next_token_xml = if truncated {
            format!("    <NextContinuationToken>{}</NextContinuationToken>\n",
                    xml_escape(&last_key))
        } else {
            String::new()
        };

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
// DeleteObjects  POST /s3/{bucket}?delete
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

    struct ObjReq {
        key: String,
        version_id: Option<String>,
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
            if let Some(key) = extract_xml_tag(obj_body, "Key").map(|k| xml_unescape(&k)) {
                objs.push(ObjReq {
                    key,
                    version_id: extract_xml_tag(obj_body, "VersionId"),
                    etag: extract_xml_tag(obj_body, "ETag"),
                    last_modified_time: extract_xml_tag(obj_body, "LastModifiedTime"),
                    size: extract_xml_tag(obj_body, "Size").and_then(|s| s.parse::<u64>().ok()),
                });
            }
            remaining = &remaining[obj_end..];
        }
        objs
    };

    if objects.len() > 1000 {
        return Ok(s3_error(StatusCode::BAD_REQUEST, "MalformedXML",
                           "The XML you provided was not well-formed or did not validate \
                            against our published schema. The number of keys in the request \
                            exceeds the maximum allowed.", &bucket));
    }

    let context = crate::service::user_context::UserContext::with_bucket(
        auth_result.user_id.clone(), auth_result.bucket.clone()
    );
    let storage_service = crate::service::storage_service::StorageService::new();

    let mut deleted_xml = String::new();
    let mut errors_xml = String::new();

    let bypass_governance = req.headers()
        .get("x-amz-bypass-governance-retention")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    for obj_req in &objects {
        let key = &obj_req.key;

        if let Some(ref vid) = obj_req.version_id {
            // Check object lock before deleting
            let (ret_blocked, hold_blocked) = db.check_object_lock_protection(&bucket, key, vid, bypass_governance)?;
            if ret_blocked || hold_blocked {
                errors_xml.push_str(&format!(
                    "    <Error><Key>{k}</Key><VersionId>{v}</VersionId>\
                     <Code>AccessDenied</Code>\
                     <Message>Object is locked and cannot be deleted</Message></Error>\n",
                    k = xml_escape(key), v = xml_escape(vid),
                ));
                continue;
            }

            if let Ok(ver_meta) = db.get_object_version(&bucket, key, vid) {
                let extents = ver_meta.to_offset_size_list();
                if !extents.is_empty() {
                    let _ = storage_service.delete_object(&context, key);
                    db.queue_deletion(&bucket, key, &extents).ok();
                }
            }
            match db.delete_specific_version(&bucket, key, vid) {
                Ok(result) => {
                    if result.was_delete_marker {
                        deleted_xml.push_str(&format!(
                            "    <Deleted><Key>{k}</Key><VersionId>{v}</VersionId>\
                             <DeleteMarker>true</DeleteMarker><DeleteMarkerVersionId>{v}</DeleteMarkerVersionId></Deleted>\n",
                            k = xml_escape(key), v = xml_escape(&result.version_id),
                        ));
                    } else {
                        deleted_xml.push_str(&format!(
                            "    <Deleted><Key>{k}</Key><VersionId>{v}</VersionId></Deleted>\n",
                            k = xml_escape(key), v = xml_escape(&result.version_id),
                        ));
                    }
                }
                Err(_) => {
                    deleted_xml.push_str(&format!(
                        "    <Deleted><Key>{}</Key></Deleted>\n", xml_escape(key),
                    ));
                }
            }
            continue;
        }

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

        use crate::metadata::sqlite_store::VersioningDeleteResult;
        match db.delete_object_v2(&bucket, key) {
            Ok(VersioningDeleteResult::Marker { version_id }) => {
                deleted_xml.push_str(&format!(
                    "    <Deleted><Key>{k}</Key><DeleteMarker>true</DeleteMarker>\
                     <DeleteMarkerVersionId>{v}</DeleteMarkerVersionId></Deleted>\n",
                    k = xml_escape(key), v = xml_escape(&version_id),
                ));
            }
            Ok(VersioningDeleteResult::Deleted) => {
                if db.check_key(&bucket, key).unwrap_or(false) {
                    storage_service.delete_object(&context, key).ok();
                }
                db.delete_metadata(&bucket, key).ok();
                deleted_xml.push_str(&format!(
                    "    <Deleted><Key>{}</Key></Deleted>\n", xml_escape(key),
                ));
            }
            Err(_) => {
                deleted_xml.push_str(&format!(
                    "    <Deleted><Key>{}</Key></Deleted>\n", xml_escape(key),
                ));
            }
        }
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
