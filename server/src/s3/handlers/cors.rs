// CORS types, helpers, and handlers.
use actix_web::{HttpRequest, HttpResponse, Error, http::StatusCode};

use super::common::*;

pub(super) struct CorsRule {
    pub(super) allowed_origins: Vec<String>,
    pub(super) allowed_methods: Vec<String>,
    pub(super) allowed_headers: Vec<String>,
    pub(super) expose_headers:  Vec<String>,
    pub(super) max_age_seconds: Option<u32>,
}

pub(super) fn parse_cors_rules(xml: &str) -> Vec<CorsRule> {
    extract_all_xml_tags(xml, "CORSRule").into_iter().map(|block| {
        CorsRule {
            allowed_origins: extract_all_xml_tags(&block, "AllowedOrigin"),
            allowed_methods: extract_all_xml_tags(&block, "AllowedMethod"),
            allowed_headers: extract_all_xml_tags(&block, "AllowedHeader"),
            expose_headers:  extract_all_xml_tags(&block, "ExposeHeader"),
            max_age_seconds: extract_xml_tag(&block, "MaxAgeSeconds")
                .and_then(|s| s.trim().parse().ok()),
        }
    }).collect()
}

/// Glob-match an origin against an S3 AllowedOrigin pattern (single '*' wildcard).
pub(super) fn origin_matches_pattern(origin: &str, pattern: &str) -> bool {
    if pattern == "*" { return true; }
    match pattern.find('*') {
        None      => origin == pattern,
        Some(pos) => {
            let prefix = &pattern[..pos];
            let suffix = &pattern[pos + 1..];
            origin.len() >= prefix.len() + suffix.len()
                && origin.starts_with(prefix)
                && origin.ends_with(suffix)
        }
    }
}

/// Find the first CORS rule matching origin + method + requested headers.
pub(super) fn find_cors_match<'a>(
    rules: &'a [CorsRule],
    origin: &str,
    method: &str,
    req_headers: &[&str],
) -> Option<&'a CorsRule> {
    for rule in rules {
        if !rule.allowed_origins.iter().any(|p| origin_matches_pattern(origin, p)) {
            continue;
        }
        if !rule.allowed_methods.iter().any(|m| m.eq_ignore_ascii_case(method)) {
            continue;
        }
        if !req_headers.is_empty() {
            let ok = req_headers.iter().all(|rh| {
                rule.allowed_headers.iter().any(|ah| ah == "*" || ah.eq_ignore_ascii_case(rh))
            });
            if !ok { continue; }
        }
        return Some(rule);
    }
    None
}

/// The Allow-Origin value to echo back.
pub(super) fn cors_allow_origin<'a>(origin: &'a str, matched_pattern: &str) -> &'a str {
    if matched_pattern == "*" { "*" } else { origin }
}

// ---------------------------------------------------------------------------
// GetBucketLocation  GET /s3/{bucket}?location
// ---------------------------------------------------------------------------

pub(super) async fn s3_get_bucket_location_inner(bucket: &str, req: &HttpRequest) -> Result<HttpResponse, Error> {
    use crate::s3::auth::authenticate_s3_request;
    use crate::service::metadata_service::MetadataService;

    let auth_result = authenticate_s3_request(req).await?;
    let db = MetadataService::new(&auth_result.user_id)?;
    if let Err(resp) = require_bucket(&db, bucket) { return Ok(resp); }

    let location = db.get_bucket_location(bucket)?;
    let loc_xml = if location.is_empty() {
        "<LocationConstraint/>".to_string()
    } else {
        format!("<LocationConstraint>{}</LocationConstraint>", xml_escape(&location))
    };

    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         {loc}",
        loc = loc_xml,
    );
    Ok(HttpResponse::Ok().content_type("application/xml").body(xml))
}

// ---------------------------------------------------------------------------
// GetBucketCors  GET /s3/{bucket}?cors
// ---------------------------------------------------------------------------

pub(super) async fn s3_get_bucket_cors_inner(bucket: &str, req: &HttpRequest) -> Result<HttpResponse, Error> {
    use crate::s3::auth::authenticate_s3_request;
    use crate::service::metadata_service::MetadataService;

    let auth_result = authenticate_s3_request(req).await?;
    let db = MetadataService::new(&auth_result.user_id)?;
    if let Err(resp) = require_bucket(&db, bucket) { return Ok(resp); }

    match db.get_bucket_cors(bucket)? {
        None => Ok(s3_error(StatusCode::NOT_FOUND, "NoSuchCORSConfiguration",
                            "The CORS configuration does not exist.", bucket)),
        Some(xml) => Ok(HttpResponse::Ok()
            .content_type("application/xml")
            .body(format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n{}", xml))),
    }
}

// ---------------------------------------------------------------------------
// OPTIONS — CORS preflight handler
// ---------------------------------------------------------------------------

pub async fn s3_cors_not_configured_handler(req: HttpRequest) -> Result<HttpResponse, Error> {
    let bucket = req.match_info().get("bucket").unwrap_or("");

    let origin = match req.headers().get("origin").and_then(|v| v.to_str().ok()) {
        Some(o) => o.to_string(),
        None => return Ok(s3_error(StatusCode::BAD_REQUEST, "CORSNotEnabled",
                                   "CORS is not enabled for this bucket.", bucket)),
    };

    let request_method = match req.headers().get("access-control-request-method")
        .and_then(|v| v.to_str().ok())
    {
        Some(m) => m.to_string(),
        None => return Ok(s3_error(StatusCode::BAD_REQUEST, "CORSNotEnabled",
                                   "CORS is not enabled for this bucket.", bucket)),
    };

    let request_headers: Vec<String> = req.headers()
        .get("access-control-request-headers")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').map(|h| h.trim().to_string()).collect())
        .unwrap_or_default();
    let req_header_refs: Vec<&str> = request_headers.iter().map(|s| s.as_str()).collect();

    use crate::metadata::sqlite_store::SQLiteMetadataStore;
    let cors_xml = SQLiteMetadataStore::new().get_bucket_cors(bucket)?;

    let cors_xml = match cors_xml {
        None => return Ok(s3_error(StatusCode::BAD_REQUEST, "CORSNotEnabled",
                                   "CORS is not enabled for this bucket.", bucket)),
        Some(xml) => xml,
    };

    let rules = parse_cors_rules(&cors_xml);
    let matched = find_cors_match(&rules, &origin, &request_method, &req_header_refs);

    match matched {
        None => Ok(HttpResponse::Forbidden()
            .insert_header(("Content-Length", "0"))
            .body("")),
        Some(rule) => {
            let matched_pattern = rule.allowed_origins.iter()
                .find(|p| origin_matches_pattern(&origin, p))
                .map(|s| s.as_str())
                .unwrap_or("");
            let allow_origin = cors_allow_origin(&origin, matched_pattern);
            let allow_methods = rule.allowed_methods.join(", ");

            let mut resp = HttpResponse::Ok();
            resp.insert_header(("Access-Control-Allow-Origin", allow_origin));
            resp.insert_header(("Access-Control-Allow-Methods", allow_methods.as_str()));
            if !rule.allowed_headers.is_empty() {
                resp.insert_header(("Access-Control-Allow-Headers",
                    rule.allowed_headers.join(", ").as_str()));
            }
            if let Some(max_age) = rule.max_age_seconds {
                resp.insert_header(("Access-Control-Max-Age", max_age.to_string().as_str()));
            }
            if !rule.expose_headers.is_empty() {
                resp.insert_header(("Access-Control-Expose-Headers",
                    rule.expose_headers.join(", ").as_str()));
            }
            resp.insert_header(("Content-Length", "0"));
            Ok(resp.body(""))
        }
    }
}
