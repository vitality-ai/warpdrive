// Versioning state handlers + version-specific get/delete + list_object_versions inner.
use actix_web::{web, HttpRequest, HttpResponse, Error, http::StatusCode};

use std::collections::HashMap;

use super::common::*;

pub(super) async fn s3_put_bucket_versioning_inner(bucket: &str, body: &[u8], req: &HttpRequest) -> Result<HttpResponse, Error> {
    use crate::s3::auth::authenticate_s3_request;
    use crate::service::metadata_service::MetadataService;

    let auth_result = authenticate_s3_request(req).await?;
    let db = MetadataService::new(&auth_result.user_id)?;
    if let Err(resp) = require_bucket(&db, bucket) { return Ok(resp); }

    let xml = String::from_utf8_lossy(body);
    let state = match extract_xml_tag(&xml, "Status").as_deref() {
        Some("Enabled") => "enabled",
        Some("Suspended") => "suspended",
        _ => return Ok(s3_error(StatusCode::BAD_REQUEST, "MalformedXML",
                                "Invalid versioning configuration", bucket)),
    };
    db.set_versioning_state(bucket, state)?;
    Ok(HttpResponse::Ok().insert_header(("Content-Length", "0")).body(""))
}

pub(super) async fn s3_get_bucket_versioning_inner(bucket: &str, req: &HttpRequest) -> Result<HttpResponse, Error> {
    use crate::s3::auth::authenticate_s3_request;
    use crate::service::metadata_service::MetadataService;

    let auth_result = authenticate_s3_request(req).await?;
    let db = MetadataService::new(&auth_result.user_id)?;
    if let Err(resp) = require_bucket(&db, bucket) { return Ok(resp); }

    let state = db.get_versioning_state(bucket)?;
    let status_xml = match state.as_str() {
        "enabled"   => "<Status>Enabled</Status>",
        "suspended" => "<Status>Suspended</Status>",
        _           => "",
    };
    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <VersioningConfiguration xmlns=\"{s3}\">{status}</VersioningConfiguration>",
        s3 = S3_XMLNS, status = status_xml,
    );
    Ok(HttpResponse::Ok().content_type("application/xml").body(xml))
}

pub(super) async fn s3_delete_specific_version_handler(bucket: &str, key: &str, version_id: &str, req: &HttpRequest) -> Result<HttpResponse, Error> {
    use crate::s3::auth::authenticate_s3_request;
    use crate::service::metadata_service::MetadataService;

    let auth_result = authenticate_s3_request(req).await?;
    let db = MetadataService::new(&auth_result.user_id)?;
    if let Err(resp) = require_bucket(&db, bucket) { return Ok(resp); }

    if let Ok(ver_meta) = db.get_object_version(bucket, key, version_id) {
        let extents = ver_meta.to_offset_size_list();
        if !extents.is_empty() {
            db.queue_deletion(bucket, key, &extents)?;
        }
    }

    let result = db.delete_specific_version(bucket, key, version_id)?;

    let mut resp = HttpResponse::NoContent();
    if result.was_delete_marker {
        resp.insert_header(("x-amz-delete-marker", "true"));
    }
    resp.insert_header(("x-amz-version-id", result.version_id));
    Ok(resp.insert_header(("Content-Length", "0")).body(""))
}

pub(super) async fn s3_get_object_version_handler(bucket: &str, key: &str, version_id: &str, req: &HttpRequest) -> Result<HttpResponse, Error> {
    use crate::s3::auth::authenticate_s3_request;
    use crate::service::metadata_service::MetadataService;
    use crate::service::storage_service::{StorageService, StorageMode};
    use crate::service::user_context::UserContext;

    let resource = format!("/{}/{}", bucket, key);
    let auth_result = authenticate_s3_request(req).await?;
    let db = MetadataService::new(&auth_result.user_id)?;
    if let Err(resp) = require_bucket(&db, bucket) { return Ok(resp); }

    let meta = match db.get_object_version(bucket, key, version_id) {
        Ok(m) => m,
        Err(_) => return Ok(s3_error(StatusCode::NOT_FOUND, "NoSuchKey",
                                     "The specified key does not exist.", &resource)),
    };

    if meta.is_delete_marker {
        let mut r = s3_error(StatusCode::METHOD_NOT_ALLOWED, "MethodNotAllowed",
                             "The specified method is not allowed against this resource.", &resource);
        r.headers_mut().insert(
            actix_web::http::header::HeaderName::from_static("x-amz-delete-marker"),
            actix_web::http::header::HeaderValue::from_static("true"),
        );
        if let Some(vid) = &meta.version_id {
            r.headers_mut().insert(
                actix_web::http::header::HeaderName::from_static("x-amz-version-id"),
                actix_web::http::header::HeaderValue::from_str(vid).unwrap_or_else(|_| actix_web::http::header::HeaderValue::from_static("")),
            );
        }
        return Ok(r);
    }

    let total_size = meta.size;
    let etag = meta.etag.clone().unwrap_or_default();
    let content_type = meta.content_type.clone().unwrap_or_else(|| "application/octet-stream".into());
    let last_modified = meta.last_modified.clone().unwrap_or_default();
    let extents = meta.to_offset_size_list();

    let context = UserContext::with_bucket(
        auth_result.user_id.clone(), auth_result.bucket.clone()
    );
    let body = web::block(move || {
        StorageService::new()
            .read_object(&context, &extents, StorageMode::S3)
            .map_err(|e| e.to_string())
    }).await
    .map_err(actix_web::error::ErrorInternalServerError)?
    .map_err(actix_web::error::ErrorInternalServerError)?;

    let mut resp = HttpResponse::Ok();
    resp.insert_header(("Content-Type", content_type));
    resp.insert_header(("ETag", etag));
    resp.insert_header(("Content-Length", total_size.to_string()));
    if !last_modified.is_empty() { resp.insert_header(("Last-Modified", last_modified)); }
    if let Some(vid) = &meta.version_id { resp.insert_header(("x-amz-version-id", vid.clone())); }
    for (k, v) in &meta.user_metadata {
        resp.insert_header((format!("x-amz-meta-{}", k), metadata_value_header(v)));
    }
    Ok(resp.body(body))
}

// ---------------------------------------------------------------------------
// ListObjectVersions  GET /s3/{bucket}?versions
// ---------------------------------------------------------------------------

pub(super) async fn s3_list_object_versions_handler_inner(bucket: &str, req: &HttpRequest) -> Result<HttpResponse, Error> {
    use crate::s3::auth::authenticate_s3_request;
    use crate::service::metadata_service::MetadataService;

    let auth_result = authenticate_s3_request(req).await?;
    let db = MetadataService::new(&auth_result.user_id)?;

    if let Err(resp) = require_bucket(&db, bucket) { return Ok(resp); }

    let query = web::Query::<HashMap<String, String>>::from_query(req.query_string())
        .unwrap_or_else(|_| web::Query(HashMap::new()));
    let max_keys: usize = query.get("max-keys")
        .and_then(|s| s.parse().ok())
        .unwrap_or(1000)
        .min(1000);
    let key_marker       = query.get("key-marker").cloned().unwrap_or_default();
    let version_id_marker = query.get("version-id-marker").cloned().unwrap_or_default();
    let prefix           = query.get("prefix").cloned().unwrap_or_default();
    let delimiter        = query.get("delimiter").cloned().unwrap_or_default();
    let owner_id         = xml_escape(&auth_result.user_id);

    log::info!("S3 ListObjectVersions: bucket={}", bucket);

    let (rows, is_truncated, next_key, next_vid) = db.list_object_versions_full(
        bucket, &prefix, &key_marker, &version_id_marker, max_keys,
    )?;

    let mut common_prefixes: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut versions_xml = String::new();

    for row in &rows {
        if !delimiter.is_empty() {
            let suffix = &row.key[prefix.len()..];
            if let Some(pos) = suffix.find(&delimiter as &str) {
                let cp = format!("{}{}{}", prefix, &suffix[..pos], delimiter);
                common_prefixes.insert(cp);
                continue;
            }
        }

        let display_vid = if row.version_id.is_empty() { "null" } else { &row.version_id };
        let etag_bare = row.etag.trim_matches('"');

        if row.is_delete_marker {
            versions_xml.push_str(&format!(
                "    <DeleteMarker>\n\
                 \t<Key>{key}</Key>\n\
                 \t<VersionId>{vid}</VersionId>\n\
                 \t<IsLatest>{latest}</IsLatest>\n\
                 \t<LastModified>{lm}</LastModified>\n\
                 \t<Owner><ID>{owner}</ID><DisplayName>{owner}</DisplayName></Owner>\n\
                 \t</DeleteMarker>\n",
                key    = xml_escape(&row.key),
                vid    = xml_escape(display_vid),
                latest = row.is_latest,
                lm     = xml_escape(&row.last_modified),
                owner  = owner_id,
            ));
        } else {
            versions_xml.push_str(&format!(
                "    <Version>\n\
                 \t<Key>{key}</Key>\n\
                 \t<VersionId>{vid}</VersionId>\n\
                 \t<IsLatest>{latest}</IsLatest>\n\
                 \t<LastModified>{lm}</LastModified>\n\
                 \t<ETag>&quot;{etag}&quot;</ETag>\n\
                 \t<Size>{size}</Size>\n\
                 \t<StorageClass>STANDARD</StorageClass>\n\
                 \t<Owner><ID>{owner}</ID><DisplayName>{owner}</DisplayName></Owner>\n\
                 \t</Version>\n",
                key    = xml_escape(&row.key),
                vid    = xml_escape(display_vid),
                latest = row.is_latest,
                lm     = xml_escape(&row.last_modified),
                etag   = etag_bare,
                size   = row.size,
                owner  = owner_id,
            ));
        }
    }

    let mut cp_xml = String::new();
    for cp in &common_prefixes {
        cp_xml.push_str(&format!("    <CommonPrefixes><Prefix>{}</Prefix></CommonPrefixes>\n", xml_escape(cp)));
    }

    let truncation_xml = if is_truncated {
        format!(
            "    <NextKeyMarker>{}</NextKeyMarker>\n    <NextVersionIdMarker>{}</NextVersionIdMarker>\n",
            xml_escape(&next_key),
            xml_escape(if next_vid.is_empty() { "null" } else { &next_vid }),
        )
    } else { String::new() };

    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <ListVersionsResult xmlns=\"{s3}\">\n\
             <Name>{bucket}</Name>\n\
             <Prefix>{prefix}</Prefix>\n\
             <KeyMarker>{km}</KeyMarker>\n\
             <VersionIdMarker>{vim}</VersionIdMarker>\n\
             <MaxKeys>{max_keys}</MaxKeys>\n\
             <IsTruncated>{truncated}</IsTruncated>\n\
             {trunc_xml}{versions}{cp}\
         </ListVersionsResult>",
        s3        = S3_XMLNS,
        bucket    = xml_escape(bucket),
        prefix    = xml_escape(&prefix),
        km        = xml_escape(&key_marker),
        vim       = xml_escape(&version_id_marker),
        max_keys  = max_keys,
        truncated = is_truncated,
        trunc_xml = truncation_xml,
        versions  = versions_xml,
        cp        = cp_xml,
    );
    Ok(HttpResponse::Ok().content_type("application/xml").body(xml))
}
