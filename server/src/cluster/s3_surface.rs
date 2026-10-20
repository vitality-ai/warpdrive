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

use actix_web::error::ErrorNotFound;
use actix_web::{web, Error, HttpRequest, HttpResponse};
use std::sync::Arc;

use super::coordinator::{
    cluster_delete_object_inner, cluster_get_object_inner, cluster_put_object_inner, ClusterState,
};
use super::location_store::ObjectS3Metadata;
use super::metadata_store::ClusterMetadataStore;
use crate::s3::auth::authenticate_s3_request;
use crate::s3::handlers::common::{md5_etag, normalize_etag, s3_precondition_failed, HeadBody};
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

    let mut resp = cluster_put_object_inner(&user_id, bucket, key, body, &req, &state, &s3_meta).await?;
    if let Ok(hv) = actix_web::http::header::HeaderValue::from_str(&etag) {
        resp.headers_mut().insert(actix_web::http::header::HeaderName::from_static("etag"), hv);
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
    cluster_get_object_inner(&auth_result.user_id, bucket, key, &req, &state).await
}

/// DELETE, authenticated: delegates straight to `cluster_delete_object_inner`.
pub async fn cluster_s3_delete_object_authenticated(
    path: web::Path<(String, String)>,
    req: HttpRequest,
    state: web::Data<ClusterState>,
) -> Result<HttpResponse, Error> {
    let (bucket, key) = path.into_inner();
    let auth_result = authenticate_s3_request(&req).await?;
    cluster_delete_object_inner(&auth_result.user_id, bucket, key, &state).await
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
