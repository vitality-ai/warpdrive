// Object Lock handlers — bucket config, per-object retention, legal hold.
use actix_web::{HttpRequest, HttpResponse, Error, http::StatusCode};

use crate::s3::auth::authenticate_s3_request;
use crate::service::metadata_service::MetadataService;

use super::common::*;

// ---------------------------------------------------------------------------
// Helper — compute retain-until-date from default retention config
// ---------------------------------------------------------------------------

pub fn compute_retain_until(days: Option<i64>, years: Option<i64>) -> String {
    let now = chrono::Utc::now();
    let dt = if let Some(d) = days {
        now + chrono::Duration::days(d)
    } else if let Some(y) = years {
        now + chrono::Duration::days(y * 365)
    } else {
        now
    };
    dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

// ---------------------------------------------------------------------------
// PUT /{bucket}?object-lock — set bucket-level object lock configuration
// ---------------------------------------------------------------------------

pub async fn s3_put_bucket_object_lock_inner(bucket: &str, body: &[u8], req: &HttpRequest) -> Result<HttpResponse, Error> {
    let auth_result = authenticate_s3_request(req).await?;
    let db = MetadataService::new(&auth_result.user_id)?;
    if let Err(resp) = require_bucket(&db, bucket) { return Ok(resp); }

    let xml = String::from_utf8_lossy(body);

    // ObjectLockEnabled must be "Enabled"
    let status = extract_xml_tag(&xml, "ObjectLockEnabled").unwrap_or_default();
    if status != "Enabled" {
        return Ok(s3_error(StatusCode::BAD_REQUEST, "MalformedXML",
                           "ObjectLockEnabled must be 'Enabled'", bucket));
    }

    // Bucket must have object lock enabled (set at CreateBucket time) or versioning must be enabled
    let lock_enabled = db.get_bucket_object_lock_enabled(bucket)?;
    if !lock_enabled {
        let vs = db.get_versioning_state(bucket)?;
        if vs != "enabled" {
            return Ok(s3_error(StatusCode::CONFLICT, "InvalidBucketState",
                               "Object lock can only be enabled on a versioning-enabled bucket", bucket));
        }
        db.set_bucket_object_lock_enabled(bucket, true)?;
    }

    // Parse Rule/DefaultRetention
    let mode = extract_xml_tag(&xml, "Mode").unwrap_or_default();
    if !mode.is_empty() {
        if mode != "COMPLIANCE" && mode != "GOVERNANCE" {
            return Ok(s3_error(StatusCode::BAD_REQUEST, "MalformedXML",
                               "Mode must be COMPLIANCE or GOVERNANCE", bucket));
        }
        let days_str  = extract_xml_tag(&xml, "Days").unwrap_or_default();
        let years_str = extract_xml_tag(&xml, "Years").unwrap_or_default();

        let has_days  = !days_str.is_empty();
        let has_years = !years_str.is_empty();

        if has_days && has_years {
            return Ok(s3_error(StatusCode::BAD_REQUEST, "MalformedXML",
                               "Cannot specify both Days and Years", bucket));
        }

        let days: Option<i64> = if has_days {
            match days_str.parse::<i64>() {
                Ok(d) if d > 0 => Some(d),
                _ => return Ok(s3_error(StatusCode::BAD_REQUEST, "InvalidRetentionPeriod",
                                        "Days must be a positive integer", bucket)),
            }
        } else { None };

        let years: Option<i64> = if has_years {
            match years_str.parse::<i64>() {
                Ok(y) if y > 0 => Some(y),
                _ => return Ok(s3_error(StatusCode::BAD_REQUEST, "InvalidRetentionPeriod",
                                        "Years must be a positive integer", bucket)),
            }
        } else { None };

        if days.is_none() && years.is_none() {
            return Ok(s3_error(StatusCode::BAD_REQUEST, "MalformedXML",
                               "DefaultRetention must specify Days or Years", bucket));
        }

        db.put_object_lock_config(bucket, &mode, days, years)?;
    }

    Ok(HttpResponse::Ok().insert_header(("Content-Length", "0")).body(""))
}

// ---------------------------------------------------------------------------
// GET /{bucket}?object-lock — get bucket-level object lock configuration
// ---------------------------------------------------------------------------

pub async fn s3_get_bucket_object_lock_inner(bucket: &str, req: &HttpRequest) -> Result<HttpResponse, Error> {
    let auth_result = authenticate_s3_request(req).await?;
    let db = MetadataService::new(&auth_result.user_id)?;
    if let Err(resp) = require_bucket(&db, bucket) { return Ok(resp); }

    let lock_enabled = db.get_bucket_object_lock_enabled(bucket)?;
    if !lock_enabled {
        return Ok(s3_error(StatusCode::NOT_FOUND, "ObjectLockConfigurationNotFoundError",
                           "Object Lock configuration does not exist for this bucket", bucket));
    }

    let rule_xml = match db.get_object_lock_config(bucket)? {
        Some((mode, days, years)) => {
            let period_xml = if let Some(d) = days {
                format!("<Days>{}</Days>", d)
            } else if let Some(y) = years {
                format!("<Years>{}</Years>", y)
            } else {
                String::new()
            };
            format!(
                "<Rule><DefaultRetention><Mode>{mode}</Mode>{period}</DefaultRetention></Rule>",
                mode = xml_escape(&mode), period = period_xml,
            )
        }
        None => String::new(),
    };

    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <ObjectLockConfiguration xmlns=\"{s3}\">\
         <ObjectLockEnabled>Enabled</ObjectLockEnabled>\
         {rule}\
         </ObjectLockConfiguration>",
        s3 = S3_XMLNS, rule = rule_xml,
    );
    Ok(HttpResponse::Ok().content_type("application/xml").body(xml))
}

// ---------------------------------------------------------------------------
// PUT /{bucket}/{key}?retention — set per-object retention
// ---------------------------------------------------------------------------

pub async fn s3_put_object_retention_inner(bucket: &str, key: &str, body: &[u8], req: &HttpRequest) -> Result<HttpResponse, Error> {
    let auth_result = authenticate_s3_request(req).await?;
    let db = MetadataService::new(&auth_result.user_id)?;
    if let Err(resp) = require_bucket(&db, bucket) { return Ok(resp); }

    let lock_enabled = db.get_bucket_object_lock_enabled(bucket)?;
    if !lock_enabled {
        return Ok(s3_error(StatusCode::BAD_REQUEST, "InvalidRequest",
                           "Bucket does not have object lock enabled", &format!("/{}/{}", bucket, key)));
    }

    let xml = String::from_utf8_lossy(body);
    let mode = extract_xml_tag(&xml, "Mode").unwrap_or_default();
    if mode != "COMPLIANCE" && mode != "GOVERNANCE" {
        return Ok(s3_error(StatusCode::BAD_REQUEST, "MalformedXML",
                           "Mode must be COMPLIANCE or GOVERNANCE", &format!("/{}/{}", bucket, key)));
    }

    let retain_until = extract_xml_tag(&xml, "RetainUntilDate").unwrap_or_default();
    if retain_until.is_empty() {
        return Ok(s3_error(StatusCode::BAD_REQUEST, "MalformedXML",
                           "RetainUntilDate is required", &format!("/{}/{}", bucket, key)));
    }

    let qmap = req_query_map(req);
    let version_id = qmap.get("versionId").cloned().unwrap_or_default();

    let bypass_governance = req.headers()
        .get("x-amz-bypass-governance-retention")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    let vid = if version_id.is_empty() {
        db.get_object_full(bucket, key)
            .ok()
            .and_then(|m| m.version_id)
            .unwrap_or_default()
    } else {
        version_id
    };

    // Enforce retention protection: check existing lock before overwriting
    if let Some(existing) = db.get_object_lock(bucket, key, &vid)? {
        if let (Some(ref ex_mode), Some(ref ex_until)) = (&existing.mode, &existing.retain_until_date) {
            let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
            if ex_until.as_str() > now.as_str() {
                let shortening = retain_until.as_str() < ex_until.as_str();
                let mode_changed = mode.as_str() != ex_mode.as_str();
                let blocked = match ex_mode.as_str() {
                    "COMPLIANCE" => shortening || mode_changed,
                    "GOVERNANCE" => !bypass_governance && (shortening || mode_changed),
                    _ => false,
                };
                if blocked {
                    return Ok(s3_error(StatusCode::FORBIDDEN, "AccessDenied",
                                       "Object is locked and the retention cannot be changed as requested",
                                       &format!("/{}/{}", bucket, key)));
                }
            }
        }
        if existing.legal_hold == "ON" && !bypass_governance {
            // Legal hold alone does NOT block retention changes
        }
    }

    db.put_object_lock(bucket, key, &vid, Some(&mode), Some(&retain_until), None)?;
    Ok(HttpResponse::Ok().insert_header(("Content-Length", "0")).body(""))
}

// ---------------------------------------------------------------------------
// GET /{bucket}/{key}?retention — get per-object retention
// ---------------------------------------------------------------------------

pub async fn s3_get_object_retention_inner(bucket: &str, key: &str, req: &HttpRequest) -> Result<HttpResponse, Error> {
    let auth_result = authenticate_s3_request(req).await?;
    let db = MetadataService::new(&auth_result.user_id)?;
    if let Err(resp) = require_bucket(&db, bucket) { return Ok(resp); }

    let lock_enabled = db.get_bucket_object_lock_enabled(bucket)?;
    if !lock_enabled {
        return Ok(s3_error(StatusCode::BAD_REQUEST, "InvalidRequest",
                           "Bucket does not have object lock enabled", &format!("/{}/{}", bucket, key)));
    }

    let qmap = req_query_map(req);
    let version_id = qmap.get("versionId").cloned().unwrap_or_else(|| {
        db.get_object_full(bucket, key)
            .ok()
            .and_then(|m| m.version_id)
            .unwrap_or_default()
    });

    let lock = match db.get_object_lock(bucket, key, &version_id)? {
        Some(r) => r,
        None => return Ok(s3_error(StatusCode::NOT_FOUND, "NoSuchObjectLockConfiguration",
                                   "Object does not have a retention configuration", &format!("/{}/{}", bucket, key))),
    };

    let (mode, until) = match (lock.mode, lock.retain_until_date) {
        (Some(m), Some(u)) => (m, u),
        _ => return Ok(s3_error(StatusCode::NOT_FOUND, "NoSuchObjectLockConfiguration",
                                "Object does not have a retention configuration", &format!("/{}/{}", bucket, key))),
    };

    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <Retention xmlns=\"{s3}\">\
         <Mode>{mode}</Mode>\
         <RetainUntilDate>{until}</RetainUntilDate>\
         </Retention>",
        s3 = S3_XMLNS, mode = xml_escape(&mode), until = xml_escape(&until),
    );
    Ok(HttpResponse::Ok().content_type("application/xml").body(xml))
}

// ---------------------------------------------------------------------------
// PUT /{bucket}/{key}?legal-hold — set legal hold
// ---------------------------------------------------------------------------

pub async fn s3_put_object_legal_hold_inner(bucket: &str, key: &str, body: &[u8], req: &HttpRequest) -> Result<HttpResponse, Error> {
    let auth_result = authenticate_s3_request(req).await?;
    let db = MetadataService::new(&auth_result.user_id)?;
    if let Err(resp) = require_bucket(&db, bucket) { return Ok(resp); }

    let lock_enabled = db.get_bucket_object_lock_enabled(bucket)?;
    if !lock_enabled {
        return Ok(s3_error(StatusCode::BAD_REQUEST, "InvalidRequest",
                           "Bucket does not have object lock enabled", &format!("/{}/{}", bucket, key)));
    }

    let xml = String::from_utf8_lossy(body);
    let status = extract_xml_tag(&xml, "Status").unwrap_or_default();
    if status != "ON" && status != "OFF" {
        return Ok(s3_error(StatusCode::BAD_REQUEST, "MalformedXML",
                           "Legal hold Status must be ON or OFF", &format!("/{}/{}", bucket, key)));
    }

    let qmap = req_query_map(req);
    let vid = qmap.get("versionId").cloned().unwrap_or_else(|| {
        db.get_object_full(bucket, key)
            .ok()
            .and_then(|m| m.version_id)
            .unwrap_or_default()
    });

    db.set_object_legal_hold(bucket, key, &vid, &status)?;
    Ok(HttpResponse::Ok().insert_header(("Content-Length", "0")).body(""))
}

// ---------------------------------------------------------------------------
// GET /{bucket}/{key}?legal-hold — get legal hold
// ---------------------------------------------------------------------------

pub async fn s3_get_object_legal_hold_inner(bucket: &str, key: &str, req: &HttpRequest) -> Result<HttpResponse, Error> {
    let auth_result = authenticate_s3_request(req).await?;
    let db = MetadataService::new(&auth_result.user_id)?;
    if let Err(resp) = require_bucket(&db, bucket) { return Ok(resp); }

    let lock_enabled = db.get_bucket_object_lock_enabled(bucket)?;
    if !lock_enabled {
        return Ok(s3_error(StatusCode::BAD_REQUEST, "InvalidRequest",
                           "Bucket does not have object lock enabled", &format!("/{}/{}", bucket, key)));
    }

    let qmap = req_query_map(req);
    let version_id = qmap.get("versionId").cloned().unwrap_or_else(|| {
        db.get_object_full(bucket, key)
            .ok()
            .and_then(|m| m.version_id)
            .unwrap_or_default()
    });

    let lock = db.get_object_lock(bucket, key, &version_id)?;
    let status = lock.as_ref().map(|r| r.legal_hold.as_str()).unwrap_or("OFF");

    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <LegalHold xmlns=\"{s3}\"><Status>{status}</Status></LegalHold>",
        s3 = S3_XMLNS, status = xml_escape(status),
    );
    Ok(HttpResponse::Ok().content_type("application/xml").body(xml))
}
