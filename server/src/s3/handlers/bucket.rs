// Bucket-level handlers: ListBuckets, CreateBucket, DeleteBucket, HeadBucket.
use actix_web::{web, HttpRequest, HttpResponse, Error, http::StatusCode};
use futures::StreamExt as _;
use log::info;

use std::collections::HashMap;

use crate::s3::auth::authenticate_s3_request;
use crate::service::metadata_service::MetadataService;

use super::common::*;
use super::tagging::{s3_put_bucket_tagging_inner, s3_delete_bucket_tagging_inner};
use super::versioning::s3_put_bucket_versioning_inner;
use super::acl::{s3_put_acl_stub, validate_bucket_name};

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

    let allowed_set: Option<std::collections::HashSet<&str>> = if auth_result.allow_all_buckets {
        None
    } else {
        Some(auth_result.allowed_buckets.iter().map(|s| s.as_str()).collect())
    };

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
    mut payload: web::Payload,
    req: HttpRequest,
) -> Result<HttpResponse, Error> {
    let bucket = path.into_inner();

    let qmap: HashMap<String, String> = web::Query::<HashMap<String, String>>::from_query(req.query_string())
        .map(|q| q.into_inner()).unwrap_or_default();

    if qmap.contains_key("acl") {
        return s3_put_acl_stub(&req).await;
    }
    if qmap.contains_key("tagging") || qmap.contains_key("versioning") {
        let mut body: Vec<u8> = Vec::new();
        while let Some(chunk) = payload.next().await {
            body.extend_from_slice(&chunk.map_err(actix_web::error::ErrorInternalServerError)?);
        }
        if qmap.contains_key("tagging") {
            return s3_put_bucket_tagging_inner(&bucket, &body, &req).await;
        }
        return s3_put_bucket_versioning_inner(&bucket, &body, &req).await;
    }

    let mut body_bytes: Vec<u8> = Vec::new();
    while let Some(chunk) = payload.next().await {
        let chunk = chunk.map_err(actix_web::error::ErrorInternalServerError)?;
        body_bytes.extend_from_slice(&chunk);
    }
    let body = String::from_utf8_lossy(&body_bytes);

    let query: HashMap<String, String> = web::Query::<HashMap<String, String>>::from_query(req.query_string())
        .map(|q| q.into_inner()).unwrap_or_default();

    if query.contains_key("cors") {
        let auth_result = authenticate_s3_request(&req).await?;
        let db = MetadataService::new(&auth_result.user_id)?;
        if let Err(resp) = require_bucket(&db, &bucket) { return Ok(resp); }
        db.set_bucket_cors(&bucket, body.trim())?;
        info!("S3 PutBucketCors: bucket={}", bucket);
        return Ok(HttpResponse::Ok()
            .insert_header(("Content-Length", "0"))
            .body(""));
    }

    let auth_result = authenticate_s3_request(&req).await?;
    if let Err(e) = validate_bucket_name(&bucket) { return Ok(e); }

    let db = MetadataService::new(&auth_result.user_id)?;
    db.create_bucket(&bucket)?;

    let location = extract_xml_tag(&body, "LocationConstraint")
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if !location.is_empty() {
        db.set_bucket_location(&bucket, &location)?;
    }

    info!("S3 CreateBucket: bucket={} user={} location={:?}", bucket, auth_result.user_id, location);
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

    let query: HashMap<String, String> = web::Query::<HashMap<String, String>>::from_query(req.query_string())
        .map(|q| q.into_inner()).unwrap_or_default();

    if query.contains_key("cors") {
        let auth_result = authenticate_s3_request(&req).await?;
        let db = MetadataService::new(&auth_result.user_id)?;
        if let Err(resp) = require_bucket(&db, &bucket) { return Ok(resp); }
        db.delete_bucket_cors(&bucket)?;
        info!("S3 DeleteBucketCors: bucket={}", bucket);
        return Ok(HttpResponse::NoContent().insert_header(("Content-Length", "0")).body(""));
    }

    if query.contains_key("tagging") {
        return s3_delete_bucket_tagging_inner(&bucket, &req).await;
    }

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
