//! S3-protocol surface over the distributed `/cluster/` API, mounted at
//! `/cluster/s3/...`.
//!
//! Why this exists, separately from the already-real, already-tested S3 API
//! (`s3::handlers`, SigV4 auth, bucket operations): that API writes to
//! single-node local storage only, never through `cluster_put_object`'s
//! FAC/plain dispatch — pointing a real S3 client at it would prove
//! "talks to WarpDrive," not "reconfigurability helps." This surface is
//! the other direction: real S3 protocol (GET/PUT/HEAD/DELETE/LIST) in
//! front of the *same* dispatch DuckDB's Range-GET already proved, for
//! clients like Lance's `object_store::aws` that need genuine
//! bucket/list/auth semantics, not just an arbitrary HTTP+Range URL the
//! way DuckDB's `httpfs` accepts.
//!
//! **No longer unauthenticated.** Originally built with no SigV4
//! verification at all (any `Authorization` header was accepted
//! unchecked) — the `_authenticated` functions below (and the plain
//! `cluster_s3_list_objects`) now call `authenticate_s3_request`, the
//! same real, working SigV4 implementation single-node already uses, and
//! are what `main.rs` mounts at `/cluster/s3/...`. The original
//! unauthenticated functions (`cluster_s3_head_object`, and historically
//! `cluster_put_object`/`cluster_get_object`) are kept only because the
//! separate, bare `/cluster/{bucket}/{key:.*}` routes (no `/s3/` prefix)
//! still use them and must stay unauthenticated — see each function's own
//! doc. No multipart upload, no pagination on LIST still apply; those
//! remain explicit non-goals for this surface, not gaps from this pass.

use actix_web::error::{ErrorBadRequest, ErrorNotFound};
use actix_web::{web, Error, HttpRequest, HttpResponse};
use std::sync::Arc;

use super::coordinator::{
    cluster_abort_multipart_upload_inner, cluster_complete_multipart_upload_inner,
    cluster_create_multipart_upload_inner, cluster_delete_object_inner, cluster_delete_object_tagging_inner,
    cluster_get_object_inner, cluster_get_object_legal_hold, cluster_get_object_retention, cluster_get_object_tagging,
    cluster_list_parts_inner, cluster_put_object_inner, cluster_put_object_legal_hold_inner,
    cluster_put_object_retention_inner, cluster_put_object_tagging_inner, cluster_upload_part_inner, ClusterState,
};
use super::location_store::ObjectS3Metadata;
use super::metadata_store::ClusterMetadataStore;
use crate::s3::auth::authenticate_s3_request;
use crate::s3::handlers::common::{
    extract_all_xml_tags, extract_xml_tag, md5_etag, normalize_etag, req_query_map, s3_precondition_failed, HeadBody,
    S3_XMLNS,
};
use crate::s3::handlers::tagging::{parse_tag_xml, parse_url_tags, tags_to_xml, validate_tags};
use crate::service::metadata_service::MetadataService;

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Built fresh per call -- cheap (two `Arc::clone`s of state already held
/// by `ClusterState`), and lets the auth-aware handlers below do metadata
/// *reads* (existence/etag checks for conditional requests, HEAD) through
/// the same `MetadataService` facade single-node handlers use, instead of
/// reaching into `state.location_store`/`content_location_store` by hand.
fn cluster_metadata_service(state: &web::Data<ClusterState>, user_id: &str) -> Result<MetadataService, Error> {
    let store = ClusterMetadataStore::new(state.location_store.clone(), state.content_location_store.clone());
    MetadataService::with_store(user_id, Arc::new(store))
}

/// Bare, pre-auth HEAD -- mounted at both `/cluster/{bucket}/{key:.*}`
/// (always unauthenticated, `UNAUTHENTICATED_USER`) and, historically,
/// `/cluster/s3/{bucket}/{key:.*}`. `cluster_s3_head_object_authenticated`
/// below is what `main.rs` now mounts at the latter instead -- this one's
/// behavior is otherwise completely unchanged from before this file
/// grew auth-aware siblings.
pub async fn cluster_s3_head_object(
    path: web::Path<(String, String)>,
    state: web::Data<ClusterState>,
) -> Result<HttpResponse, Error> {
    let (bucket, key) = path.into_inner();
    let user_id = super::location_store::UNAUTHENTICATED_USER;

    let (size, etag) = if let Some(record) = state.content_location_store.get(user_id, &bucket, &key) {
        (record.original_len as u64, record.etag)
    } else if let Some(record) = state.location_store.get(user_id, &bucket, &key) {
        (record.original_len as u64, record.etag)
    } else {
        return Err(ErrorNotFound("object not found"));
    };

    let mut resp = HttpResponse::Ok();
    resp.insert_header(("Accept-Ranges", "bytes")).insert_header(("Last-Modified", now_http_date()));
    if let Some(etag) = etag { resp.insert_header(("ETag", etag)); }
    Ok(resp.body(HeadBody(size)))
}

/// HEAD, authenticated: closes the same SigV4 gap as the PUT/GET/DELETE
/// handlers below. Reads via `ClusterMetadataStore` (through
/// `MetadataService`) rather than reaching into the two location stores
/// directly -- this is exactly the metadata-read use case that store
/// exists for.
pub async fn cluster_s3_head_object_authenticated(
    path: web::Path<(String, String)>,
    req: HttpRequest,
    state: web::Data<ClusterState>,
) -> Result<HttpResponse, Error> {
    let (bucket, key) = path.into_inner();
    let auth_result = authenticate_s3_request(&req).await?;
    let db = cluster_metadata_service(&state, &auth_result.user_id)?;

    let meta = db.get_object_full(&bucket, &key)?;

    let mut resp = HttpResponse::Ok();
    resp.insert_header(("Accept-Ranges", "bytes"));
    if let Some(lm) = &meta.last_modified { resp.insert_header(("Last-Modified", lm.as_str())); }
    if let Some(etag) = &meta.etag { resp.insert_header(("ETag", etag.as_str())); }
    if let Some(ct) = &meta.content_type { resp.insert_header(("Content-Type", ct.as_str())); }
    if let Some(cc) = &meta.cache_control { resp.insert_header(("Cache-Control", cc.as_str())); }
    if let Some(exp) = &meta.expires { resp.insert_header(("Expires", exp.as_str())); }
    for (k, v) in &meta.user_metadata {
        resp.insert_header((format!("x-amz-meta-{k}"), crate::s3::handlers::common::metadata_value_header(v)));
    }
    if let Ok(tags) = cluster_get_object_tagging(&auth_result.user_id, &bucket, &key, &state) {
        if !tags.is_empty() {
            resp.insert_header(("x-amz-tagging-count", tags.len().to_string()));
        }
    }
    Ok(resp.body(HeadBody(meta.size)))
}

/// PUT, authenticated: the actual byte/placement write goes through
/// `cluster_put_object_inner` directly (EC encode, placement, shard
/// fan-out, quorum, replication) -- `ClusterMetadataStore` is read-only
/// by design (see its own module doc), used here only for the
/// if-match/if-none-match existence/etag check, same as single-node's
/// `s3_put_object_handler`.
pub async fn cluster_s3_put_object_authenticated(
    path: web::Path<(String, String)>,
    body: web::Bytes,
    req: HttpRequest,
    state: web::Data<ClusterState>,
) -> Result<HttpResponse, Error> {
    let (bucket, key) = path.into_inner();
    let auth_result = authenticate_s3_request(&req).await?;
    let user_id = auth_result.user_id;

    // Query-param dispatch, same order and same query-key names as
    // single-node's `s3_put_object_handler` (object.rs), so a client sees
    // identical semantics on either surface.
    let qmap = req_query_map(&req);
    if qmap.contains_key("partNumber") && qmap.contains_key("uploadId") {
        let upload_id = qmap.get("uploadId").cloned().unwrap_or_default();
        let part_number: i32 = qmap.get("partNumber").and_then(|s| s.parse().ok())
            .ok_or_else(|| ErrorBadRequest("partNumber must be an integer"))?;
        let etag = cluster_upload_part_inner(&user_id, &bucket, &upload_id, part_number, body, &state).await?;
        let mut resp = HttpResponse::Ok();
        if let Ok(hv) = actix_web::http::header::HeaderValue::from_str(&etag) {
            resp.insert_header(("ETag", hv));
        }
        return Ok(resp.insert_header(("Content-Length", "0")).body(""));
    }
    if qmap.contains_key("tagging") {
        let xml = String::from_utf8_lossy(&body);
        let tags = parse_tag_xml(&xml);
        let resource = format!("/{bucket}/{key}");
        if let Err(resp) = validate_tags(&tags, &resource) { return Ok(resp); }
        return cluster_put_object_tagging_inner(&user_id, &bucket, &key, tags, &state).await;
    }
    if qmap.contains_key("retention") || qmap.contains_key("legal-hold") {
        let xml = String::from_utf8_lossy(&body);
        if qmap.contains_key("retention") {
            let mode = extract_xml_tag(&xml, "Mode").unwrap_or_default();
            let retain_until = extract_xml_tag(&xml, "RetainUntilDate").unwrap_or_default();
            if retain_until.is_empty() {
                return Err(ErrorBadRequest("RetainUntilDate is required"));
            }
            return cluster_put_object_retention_inner(&user_id, &bucket, &key, &mode, &retain_until, &state).await;
        }
        let status = extract_xml_tag(&xml, "Status").unwrap_or_default();
        if status != "ON" && status != "OFF" {
            return Err(ErrorBadRequest("Legal hold Status must be ON or OFF"));
        }
        return cluster_put_object_legal_hold_inner(&user_id, &bucket, &key, status == "ON", &state).await;
    }

    let if_match = req.headers().get("if-match").and_then(|v| v.to_str().ok()).map(|s| s.trim().to_string());
    let if_none_match = req.headers().get("if-none-match").and_then(|v| v.to_str().ok()).map(|s| s.trim().to_string());
    if if_match.is_some() || if_none_match.is_some() {
        let db = cluster_metadata_service(&state, &user_id)?;
        let resource = format!("/{}/{}", bucket, key);
        let obj_exists = db.check_key(&bucket, &key)?;
        let cur_etag = if obj_exists { db.get_object_full(&bucket, &key)?.etag.unwrap_or_default() } else { String::new() };
        if let Some(ref im) = if_match {
            if !obj_exists {
                return Err(ErrorNotFound("The specified key does not exist."));
            }
            if im != "*" && normalize_etag(im) != normalize_etag(&cur_etag) {
                return Ok(s3_precondition_failed(&resource));
            }
        }
        if let Some(ref inm) = if_none_match {
            if obj_exists && (inm == "*" || normalize_etag(inm) == normalize_etag(&cur_etag)) {
                return Ok(s3_precondition_failed(&resource));
            }
        }
    }

    // Same header extraction as single-node's s3_put_object_handler --
    // same header names, same aws-chunked stripping, so a client sees
    // identical PUT semantics on either surface.
    let user_metadata: std::collections::HashMap<String, String> = req.headers().iter()
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
    let content_type = req.headers().get("content-type").and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream").to_string();
    let cache_control = req.headers().get("cache-control").and_then(|v| v.to_str().ok()).map(|s| s.to_string());
    let expires = req.headers().get("expires").and_then(|v| v.to_str().ok()).map(|s| s.to_string());
    let content_encoding = req.headers().get("content-encoding").and_then(|v| v.to_str().ok()).and_then(|s| {
        let stripped: Vec<&str> = s.split(',').map(|p| p.trim())
            .filter(|p| !p.eq_ignore_ascii_case("aws-chunked")).collect();
        if stripped.is_empty() { None } else { Some(stripped.join(", ")) }
    });

    let etag = md5_etag(&body);
    let s3_meta = ObjectS3Metadata {
        etag: Some(etag.clone()),
        content_type: Some(content_type),
        last_modified: Some(now_http_date()),
        user_metadata,
        cache_control,
        expires,
        content_encoding,
        checksum_algorithm: None,
        checksum_value: None,
        checksum_type: None,
    };

    let bucket2 = bucket.clone();
    let key2 = key.clone();
    let mut resp = cluster_put_object_inner(&user_id, bucket, key, body, &req, &state, &s3_meta).await?;
    if let Ok(hv) = actix_web::http::header::HeaderValue::from_str(&etag) {
        resp.headers_mut().insert(actix_web::http::header::HeaderName::from_static("etag"), hv);
    }

    // Inline tagging/object-lock via headers on a plain PUT -- same
    // header names and same "best effort, don't fail the PUT" semantics
    // as single-node's `s3_put_object_handler` (object.rs lines ~241-280).
    if let Some(tagging_str) = req.headers().get("x-amz-tagging").and_then(|v| v.to_str().ok()) {
        let tags = parse_url_tags(tagging_str);
        let resource = format!("/{bucket2}/{key2}");
        if validate_tags(&tags, &resource).is_ok() {
            let _ = cluster_put_object_tagging_inner(&user_id, &bucket2, &key2, tags, &state).await;
        }
    }
    let obj_legal_hold = req.headers().get("x-amz-object-lock-legal-hold").and_then(|v| v.to_str().ok());
    if let Some(status) = obj_legal_hold {
        let _ = cluster_put_object_legal_hold_inner(&user_id, &bucket2, &key2, status.eq_ignore_ascii_case("ON"), &state).await;
    }
    let obj_lock_mode = req.headers().get("x-amz-object-lock-mode").and_then(|v| v.to_str().ok());
    let obj_lock_until = req.headers().get("x-amz-object-lock-retain-until-date").and_then(|v| v.to_str().ok());
    if let (Some(mode), Some(until)) = (obj_lock_mode, obj_lock_until) {
        let _ = cluster_put_object_retention_inner(&user_id, &bucket2, &key2, mode, until, &state).await;
    }

    Ok(resp)
}

/// GET, authenticated: delegates straight to `cluster_get_object_inner`,
/// which already handles Range requests and now sets ETag/Content-Type/
/// Last-Modified from the record (see coordinator.rs).
pub async fn cluster_s3_get_object_authenticated(
    path: web::Path<(String, String)>,
    req: HttpRequest,
    state: web::Data<ClusterState>,
) -> Result<HttpResponse, Error> {
    let (bucket, key) = path.into_inner();
    let auth_result = authenticate_s3_request(&req).await?;
    let user_id = auth_result.user_id;

    // Same query-param dispatch order as single-node's
    // `s3_get_object_handler`.
    let qmap = req_query_map(&req);
    if qmap.contains_key("tagging") {
        let tags = cluster_get_object_tagging(&user_id, &bucket, &key, &state)?;
        return Ok(HttpResponse::Ok().content_type("application/xml").body(tags_to_xml(&tags)));
    }
    if qmap.contains_key("retention") {
        let (mode, until) = cluster_get_object_retention(&user_id, &bucket, &key, &state)?
            .ok_or_else(|| ErrorNotFound("object does not have a retention configuration"))?;
        let xml = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Retention xmlns=\"{S3_XMLNS}\"><Mode>{mode}</Mode><RetainUntilDate>{until}</RetainUntilDate></Retention>",
        );
        return Ok(HttpResponse::Ok().content_type("application/xml").body(xml));
    }
    if qmap.contains_key("legal-hold") {
        let on = cluster_get_object_legal_hold(&user_id, &bucket, &key, &state)?;
        let status = if on { "ON" } else { "OFF" };
        let xml = format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<LegalHold xmlns=\"{S3_XMLNS}\"><Status>{status}</Status></LegalHold>");
        return Ok(HttpResponse::Ok().content_type("application/xml").body(xml));
    }
    if let Some(upload_id) = qmap.get("uploadId") {
        if !qmap.contains_key("partNumber") {
            let parts = cluster_list_parts_inner(&user_id, &bucket, upload_id, &state)?;
            let parts_xml: String = parts.iter().map(|(part_number, etag, size)| {
                format!("<Part><PartNumber>{part_number}</PartNumber><ETag>{etag}</ETag><Size>{size}</Size></Part>")
            }).collect();
            let xml = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<ListPartsResult xmlns=\"{S3_XMLNS}\"><Bucket>{bucket}</Bucket><Key>{key}</Key><UploadId>{upload_id}</UploadId>{parts_xml}</ListPartsResult>",
            );
            return Ok(HttpResponse::Ok().content_type("application/xml").body(xml));
        }
    }

    let mut resp = cluster_get_object_inner(&user_id, bucket.clone(), key.clone(), &req, &state).await?;
    if let Ok(tags) = cluster_get_object_tagging(&user_id, &bucket, &key, &state) {
        if !tags.is_empty() {
            if let Ok(hv) = actix_web::http::header::HeaderValue::from_str(&tags.len().to_string()) {
                resp.headers_mut().insert(actix_web::http::header::HeaderName::from_static("x-amz-tagging-count"), hv);
            }
        }
    }
    Ok(resp)
}

/// DELETE, authenticated: delegates straight to `cluster_delete_object_inner`,
/// except for the multipart-abort and object-tagging-delete query-param
/// dispatches single-node's `s3_delete_object_handler` also has.
pub async fn cluster_s3_delete_object_authenticated(
    path: web::Path<(String, String)>,
    req: HttpRequest,
    state: web::Data<ClusterState>,
) -> Result<HttpResponse, Error> {
    let (bucket, key) = path.into_inner();
    let auth_result = authenticate_s3_request(&req).await?;
    let user_id = auth_result.user_id;

    let qmap = req_query_map(&req);
    if let Some(upload_id) = qmap.get("uploadId") {
        cluster_abort_multipart_upload_inner(&user_id, &bucket, upload_id, &state).await?;
        return Ok(HttpResponse::NoContent().finish());
    }
    if qmap.contains_key("tagging") {
        return cluster_delete_object_tagging_inner(&user_id, &bucket, &key, &state).await;
    }

    cluster_delete_object_inner(&user_id, bucket, key, &state).await
}

/// The cluster path never tracked/emitted Last-Modified, which the
/// `s3::handlers` legacy path does (see common.rs's
/// `last_modified_now`/`_for_header`). Real S3 SDK clients (minio-go, used
/// by minio/warp) hard-require a parseable Last-Modified on every GET/HEAD
/// response and error out client-side if it's missing. "Now" isn't the
/// object's real creation time (the cluster path doesn't store one yet),
/// but is enough to unblock SDK-based tooling against this surface; a real
/// fix would thread an actual stored timestamp through, same as common.rs.
pub(crate) fn now_http_date() -> String {
    chrono::Utc::now().format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

/// A minimal ListObjectsV2, authenticated: closes the same SigV4 gap as
/// the other `/cluster/s3/...` handlers. No pagination (fine for a
/// demo-scale dataset — a Lance dataset here is tens to low hundreds of
/// files, not millions), merging both stores since a bucket's objects can
/// be split across the plain and content-dependent paths depending on
/// what each PUT opted into.
pub async fn cluster_s3_list_objects(
    path: web::Path<String>,
    query: web::Query<std::collections::HashMap<String, String>>,
    req: HttpRequest,
    state: web::Data<ClusterState>,
) -> Result<HttpResponse, Error> {
    let bucket = path.into_inner();
    let prefix = query.get("prefix").cloned().unwrap_or_default();
    let auth_result = authenticate_s3_request(&req).await?;
    let user_id = &auth_result.user_id;

    let mut entries = state.location_store.list(user_id, &bucket, &prefix);
    entries.extend(state.content_location_store.list(user_id, &bucket, &prefix));
    entries.sort();
    entries.dedup();

    let contents: String = entries
        .iter()
        .map(|(key, size)| {
            format!(
                "<Contents><Key>{}</Key><Size>{size}</Size><LastModified>1970-01-01T00:00:00.000Z</LastModified>\
                 <ETag>&quot;0&quot;</ETag><StorageClass>STANDARD</StorageClass></Contents>",
                xml_escape(key)
            )
        })
        .collect();

    let body = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\n\
           <Name>{}</Name>\n\
           <Prefix>{}</Prefix>\n\
           <KeyCount>{}</KeyCount>\n\
           <MaxKeys>1000</MaxKeys>\n\
           <IsTruncated>false</IsTruncated>\n\
           {}\n\
         </ListBucketResult>",
        xml_escape(&bucket),
        xml_escape(&prefix),
        entries.len(),
        contents
    );

    Ok(HttpResponse::Ok().content_type("application/xml").body(body))
}

/// POST, authenticated: `?uploads` -> CreateMultipartUpload, `?uploadId`
/// -> CompleteMultipartUpload. Same two-way dispatch as single-node's
/// `s3_multipart_router` (multipart.rs).
pub async fn cluster_s3_multipart_post_authenticated(
    path: web::Path<(String, String)>,
    body: web::Bytes,
    req: HttpRequest,
    state: web::Data<ClusterState>,
) -> Result<HttpResponse, Error> {
    let (bucket, key) = path.into_inner();
    let auth_result = authenticate_s3_request(&req).await?;
    let user_id = auth_result.user_id;
    let qmap = req_query_map(&req);

    if qmap.contains_key("uploads") {
        let content_type = req.headers().get("content-type").and_then(|v| v.to_str().ok())
            .filter(|s| !s.is_empty()).map(|s| s.to_string());
        let user_metadata: std::collections::HashMap<String, String> = req.headers().iter()
            .filter_map(|(name, value)| {
                let n = name.as_str().to_lowercase();
                if n.starts_with("x-amz-meta-") {
                    Some((n.trim_start_matches("x-amz-meta-").to_string(), String::from_utf8_lossy(value.as_bytes()).trim().to_string()))
                } else { None }
            })
            .collect();
        let upload_id = cluster_create_multipart_upload_inner(&user_id, &bucket, &key, content_type, user_metadata, &state).await?;
        let xml = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<InitiateMultipartUploadResult xmlns=\"{S3_XMLNS}\">\
             <Bucket>{bucket}</Bucket><Key>{key}</Key><UploadId>{upload_id}</UploadId></InitiateMultipartUploadResult>",
        );
        return Ok(HttpResponse::Ok().content_type("application/xml").body(xml));
    }

    if let Some(upload_id) = qmap.get("uploadId") {
        let xml = String::from_utf8_lossy(&body);
        let part_blocks = extract_all_xml_tags(&xml, "Part");
        let mut requested: Vec<(i32, String)> = Vec::new();
        for block in &part_blocks {
            let n = extract_xml_tag(block, "PartNumber").and_then(|s| s.trim().parse::<i32>().ok());
            let e = extract_xml_tag(block, "ETag").map(|s| s.trim().to_string());
            if let (Some(n), Some(e)) = (n, e) {
                requested.push((n, e));
            }
        }
        requested.sort_by_key(|(n, _)| *n);
        if requested.is_empty() {
            return Err(ErrorBadRequest("the XML you provided was not well-formed or did not validate"));
        }
        let (final_etag, _size) = cluster_complete_multipart_upload_inner(&user_id, &bucket, &key, upload_id, &requested, &state).await?;
        let xml = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<CompleteMultipartUploadResult xmlns=\"{S3_XMLNS}\">\
             <Bucket>{bucket}</Bucket><Key>{key}</Key><ETag>{final_etag}</ETag></CompleteMultipartUploadResult>",
        );
        return Ok(HttpResponse::Ok().content_type("application/xml").body(xml));
    }

    Err(ErrorBadRequest("invalid multipart operation"))
}
