// Tag helpers and bucket/object tagging inner handlers.
use actix_web::{HttpRequest, HttpResponse, Error, http::StatusCode};

use super::common::*;

// ---------------------------------------------------------------------------
// Tagging helpers
// ---------------------------------------------------------------------------

pub(super) fn parse_url_tags(s: &str) -> Vec<(String, String)> {
    s.split('&').filter_map(|pair| {
        if pair.is_empty() { return None; }
        let mut parts = pair.splitn(2, '=');
        let k = percent_decode(parts.next().unwrap_or(""));
        let v = percent_decode(parts.next().unwrap_or(""));
        if k.is_empty() { None } else { Some((k, v)) }
    }).collect()
}

pub(super) fn parse_tag_xml(xml: &str) -> Vec<(String, String)> {
    extract_all_xml_tags(xml, "Tag").into_iter().filter_map(|block| {
        let k = extract_xml_tag(&block, "Key")?;
        let v = extract_xml_tag(&block, "Value").unwrap_or_default();
        Some((k.trim().to_string(), v))
    }).collect()
}

pub(super) fn validate_tags(tags: &[(String, String)], resource: &str) -> Result<(), HttpResponse> {
    if tags.len() > 10 {
        return Err(s3_error(StatusCode::BAD_REQUEST, "InvalidTag",
                            "Object tag count cannot be greater than 10", resource));
    }
    for (k, v) in tags {
        if k.len() > 128 {
            return Err(s3_error(StatusCode::BAD_REQUEST, "InvalidTag",
                                "The tag key you have provided is invalid", resource));
        }
        if v.len() > 256 {
            return Err(s3_error(StatusCode::BAD_REQUEST, "InvalidTag",
                                "The tag value you have provided is invalid", resource));
        }
    }
    Ok(())
}

pub(super) fn tags_to_xml(tags: &[(String, String)]) -> String {
    let mut xml = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Tagging><TagSet>".to_string();
    for (k, v) in tags {
        xml.push_str(&format!("<Tag><Key>{}</Key><Value>{}</Value></Tag>",
            xml_escape(k), xml_escape(v)));
    }
    xml.push_str("</TagSet></Tagging>");
    xml
}

// ---------------------------------------------------------------------------
// Bucket tagging inner handlers
// ---------------------------------------------------------------------------

pub(super) async fn s3_put_bucket_tagging_inner(bucket: &str, body: &[u8], req: &HttpRequest) -> Result<HttpResponse, Error> {
    use crate::s3::auth::authenticate_s3_request;
    use crate::service::metadata_service::MetadataService;

    let auth_result = authenticate_s3_request(req).await?;
    let db = MetadataService::new(&auth_result.user_id)?;
    if let Err(resp) = require_bucket(&db, bucket) { return Ok(resp); }

    let xml = String::from_utf8_lossy(body);
    let tags = parse_tag_xml(&xml);
    if let Err(resp) = validate_tags(&tags, bucket) { return Ok(resp); }

    db.set_bucket_tags(bucket, &tags)?;
    Ok(HttpResponse::NoContent().insert_header(("Content-Length", "0")).body(""))
}

pub(super) async fn s3_get_bucket_tagging_inner(bucket: &str, req: &HttpRequest) -> Result<HttpResponse, Error> {
    use crate::s3::auth::authenticate_s3_request;
    use crate::service::metadata_service::MetadataService;

    let auth_result = authenticate_s3_request(req).await?;
    let db = MetadataService::new(&auth_result.user_id)?;
    if let Err(resp) = require_bucket(&db, bucket) { return Ok(resp); }

    let tags = db.get_bucket_tags(bucket)?;
    if tags.is_empty() {
        return Ok(s3_error(StatusCode::NOT_FOUND, "NoSuchTagSet",
                           "The TagSet does not exist", bucket));
    }
    Ok(HttpResponse::Ok().content_type("application/xml").body(tags_to_xml(&tags)))
}

pub(super) async fn s3_delete_bucket_tagging_inner(bucket: &str, req: &HttpRequest) -> Result<HttpResponse, Error> {
    use crate::s3::auth::authenticate_s3_request;
    use crate::service::metadata_service::MetadataService;

    let auth_result = authenticate_s3_request(req).await?;
    let db = MetadataService::new(&auth_result.user_id)?;
    if let Err(resp) = require_bucket(&db, bucket) { return Ok(resp); }

    db.delete_bucket_tags(bucket)?;
    Ok(HttpResponse::NoContent().insert_header(("Content-Length", "0")).body(""))
}

// ---------------------------------------------------------------------------
// Object tagging inner handlers
// ---------------------------------------------------------------------------

pub(super) async fn s3_put_object_tagging_inner(bucket: &str, key: &str, body: &[u8], req: &HttpRequest) -> Result<HttpResponse, Error> {
    use crate::s3::auth::authenticate_s3_request;
    use crate::service::metadata_service::MetadataService;

    let auth_result = authenticate_s3_request(req).await?;
    let db = MetadataService::new(&auth_result.user_id)?;
    if let Err(resp) = require_bucket(&db, bucket) { return Ok(resp); }

    let resource = format!("/{}/{}", bucket, key);
    if !db.check_key(bucket, key)? {
        return Ok(s3_error(StatusCode::NOT_FOUND, "NoSuchKey",
                           "The specified key does not exist.", &resource));
    }

    let xml = String::from_utf8_lossy(body);
    let tags = parse_tag_xml(&xml);
    if let Err(resp) = validate_tags(&tags, &resource) { return Ok(resp); }

    db.set_object_tags(bucket, key, &tags)?;
    Ok(HttpResponse::Ok().insert_header(("Content-Length", "0")).body(""))
}

pub(super) async fn s3_get_object_tagging_inner(bucket: &str, key: &str, req: &HttpRequest) -> Result<HttpResponse, Error> {
    use crate::s3::auth::authenticate_s3_request;
    use crate::service::metadata_service::MetadataService;

    let auth_result = authenticate_s3_request(req).await?;
    let db = MetadataService::new(&auth_result.user_id)?;
    if let Err(resp) = require_bucket(&db, bucket) { return Ok(resp); }

    let resource = format!("/{}/{}", bucket, key);
    if !db.check_key(bucket, key)? {
        return Ok(s3_error(StatusCode::NOT_FOUND, "NoSuchKey",
                           "The specified key does not exist.", &resource));
    }

    let tags = db.get_object_tags(bucket, key)?;
    Ok(HttpResponse::Ok().content_type("application/xml").body(tags_to_xml(&tags)))
}

pub(super) async fn s3_delete_object_tagging_inner(bucket: &str, key: &str, req: &HttpRequest) -> Result<HttpResponse, Error> {
    use crate::s3::auth::authenticate_s3_request;
    use crate::service::metadata_service::MetadataService;

    let auth_result = authenticate_s3_request(req).await?;
    let db = MetadataService::new(&auth_result.user_id)?;
    if let Err(resp) = require_bucket(&db, bucket) { return Ok(resp); }

    db.delete_object_tags(bucket, key)?;
    Ok(HttpResponse::NoContent().insert_header(("Content-Length", "0")).body(""))
}
