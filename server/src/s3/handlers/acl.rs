// ACL stub handlers + validate_bucket_name + validate_object_key.
use actix_web::{HttpRequest, HttpResponse, Error, http::StatusCode};

use super::common::*;

pub(super) async fn s3_get_object_acl_stub(bucket: &str, key: &str, req: &HttpRequest) -> Result<HttpResponse, Error> {
    use crate::s3::auth::authenticate_s3_request;
    use crate::service::metadata_service::MetadataService;

    let auth_result = authenticate_s3_request(req).await?;
    let db = MetadataService::new(&auth_result.user_id)?;
    if let Err(resp) = require_bucket(&db, bucket) { return Ok(resp); }
    if !db.check_key(bucket, key)? {
        return Ok(s3_error(StatusCode::NOT_FOUND, "NoSuchKey",
                           "The specified key does not exist.", &format!("/{}/{}", bucket, key)));
    }
    let owner_id = xml_escape(&auth_result.user_id);
    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <AccessControlPolicy xmlns=\"{s3}\">\n\
           <Owner><ID>{id}</ID><DisplayName>{id}</DisplayName></Owner>\n\
           <AccessControlList>\n\
             <Grant>\n\
               <Grantee xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" xsi:type=\"CanonicalUser\">\
                 <ID>{id}</ID><DisplayName>{id}</DisplayName></Grantee>\n\
               <Permission>FULL_CONTROL</Permission>\n\
             </Grant>\n\
           </AccessControlList>\n\
         </AccessControlPolicy>",
        s3 = S3_XMLNS, id = owner_id,
    );
    Ok(HttpResponse::Ok().content_type("application/xml").body(xml))
}

pub(super) async fn s3_get_bucket_acl_stub(bucket: &str, req: &HttpRequest) -> Result<HttpResponse, Error> {
    use crate::s3::auth::authenticate_s3_request;
    use crate::service::metadata_service::MetadataService;

    let auth_result = authenticate_s3_request(req).await?;
    let db = MetadataService::new(&auth_result.user_id)?;
    if let Err(resp) = require_bucket(&db, bucket) { return Ok(resp); }
    let owner_id = xml_escape(&auth_result.user_id);
    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <AccessControlPolicy xmlns=\"{s3}\">\n\
           <Owner><ID>{id}</ID><DisplayName>{id}</DisplayName></Owner>\n\
           <AccessControlList>\n\
             <Grant>\n\
               <Grantee xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" xsi:type=\"CanonicalUser\">\
                 <ID>{id}</ID><DisplayName>{id}</DisplayName></Grantee>\n\
               <Permission>FULL_CONTROL</Permission>\n\
             </Grant>\n\
           </AccessControlList>\n\
         </AccessControlPolicy>",
        s3 = S3_XMLNS, id = owner_id,
    );
    Ok(HttpResponse::Ok().content_type("application/xml").body(xml))
}

/// PUT ?acl on bucket or object — accept and ignore.
pub(super) async fn s3_put_acl_stub(req: &HttpRequest) -> Result<HttpResponse, Error> {
    use crate::s3::auth::authenticate_s3_request;
    let _auth = authenticate_s3_request(req).await?;
    Ok(HttpResponse::Ok().insert_header(("Content-Length", "0")).body(""))
}

/// Validate S3 bucket name rules.
pub(super) fn validate_bucket_name(bucket: &str) -> Result<(), HttpResponse> {
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

/// Reject keys containing C0/C1 control characters.
pub(super) fn validate_object_key(key: &str, bucket: &str) -> Result<(), HttpResponse> {
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
