// Author: Tejus Chandrashekar
// POST Object (RFC 2.14): POST /s3/{bucket} with multipart/form-data,
// authorized by a signed policy document rather than a per-request
// Authorization header -- the mechanism HTML forms use to upload directly
// to S3 without ever holding real credentials in the browser.
//
// This is the classic (SigV2-era) POST policy scheme, not SigV4: the Ceph
// `s3-tests` suite's own `test_post_object_*` tests sign with
// `AWSAccessKeyId` + `signature = base64(HMAC-SHA1(secret_key, policy_b64))`,
// not `x-amz-credential`/`x-amz-signature` (confirmed by reading the actual
// test source, since this project's own RFC2.14 doc guessed the SigV4 shape
// without checking). AWS still accepts this form today for POST policies;
// it isn't the same thing as this project's "SigV2 header auth is
// deprecated, not implemented" stance, which is about regular
// Authorization-header requests, a different mechanism.
use actix_multipart::Multipart;
use actix_web::{web, HttpRequest, HttpResponse, Error, http::StatusCode};
use base64::Engine as _;
use futures::StreamExt as _;
use hmac::{Hmac, Mac};
use sha1::Sha1;
use std::collections::HashMap;

use super::common::*;
use crate::metadata::Metadata;
use crate::service::metadata_service::MetadataService;
use crate::storage::config::StorageConfig;
use super::tagging::{parse_tag_xml, validate_tags};

type HmacSha1 = Hmac<Sha1>;

struct ParsedForm {
    /// Every non-file field, lowercased name -> value. Multipart field
    /// names are matched case-insensitively throughout this handler (the
    /// Ceph suite's `test_post_object_case_insensitive_condition_fields`
    /// sends `kEy`/`aCl`/`pOLICy`), so normalizing once here means every
    /// later lookup is a plain lowercase key.
    fields: HashMap<String, String>,
    file_bytes: Vec<u8>,
    file_name: Option<String>,
}

async fn parse_multipart_form(mut payload: Multipart) -> Result<ParsedForm, Error> {
    let mut fields = HashMap::new();
    let mut file_bytes = Vec::new();
    let mut file_name = None;

    while let Some(field_result) = payload.next().await {
        let mut field = field_result.map_err(|e| ErrorBadRequestMultipart(e.to_string()))?;
        let (name, filename) = {
            let cd = field.content_disposition();
            let name = cd.get_name().unwrap_or("").to_string();
            let filename = cd.get_filename().map(|s| s.to_string());
            (name, filename)
        };
        let mut bytes = Vec::new();
        while let Some(chunk) = field.next().await {
            let chunk = chunk.map_err(|e| ErrorBadRequestMultipart(e.to_string()))?;
            bytes.extend_from_slice(&chunk);
        }

        if name.eq_ignore_ascii_case("file") {
            file_name = filename;
            file_bytes = bytes;
        } else {
            let value = String::from_utf8_lossy(&bytes).to_string();
            fields.insert(name.to_lowercase(), value);
        }
    }

    Ok(ParsedForm { fields, file_bytes, file_name })
}

#[allow(non_snake_case)]
fn ErrorBadRequestMultipart(msg: String) -> Error {
    actix_web::error::ErrorBadRequest(msg)
}

/// A policy condition, normalized from any of the three JSON shapes the
/// spec allows: `{"field": "value"}` (eq shorthand), `["eq"|"starts-with",
/// "$field", "value"]`, or `["content-length-range", min, max]`.
enum Condition {
    Eq { field: String, value: String },
    StartsWith { field: String, prefix: String },
    ContentLengthRange { min: i64, max: i64 },
}

/// Parses the policy document's `conditions` array into `Condition`s.
/// Any entry that doesn't match one of the three well-formed shapes is a
/// structural problem with the policy itself (distinct from a condition
/// that's well-formed but doesn't match the request) -- callers turn this
/// into `400`, matching `test_post_object_empty_conditions` (a `{}` with
/// zero keys), `test_post_object_missing_content_length_argument`
/// (`["content-length-range", 0]`, wrong arity), and
/// `test_post_object_invalid_content_length_argument` (negative bound).
fn parse_conditions(conditions: &[serde_json::Value]) -> Result<Vec<Condition>, String> {
    let mut out = Vec::with_capacity(conditions.len());
    for c in conditions {
        match c {
            serde_json::Value::Object(map) => {
                if map.len() != 1 {
                    return Err(format!("malformed condition object (expected exactly 1 key): {:?}", map));
                }
                let (k, v) = map.iter().next().unwrap();
                let value = v.as_str().ok_or_else(|| format!("condition value for {k} must be a string"))?;
                out.push(Condition::Eq { field: k.to_lowercase(), value: value.to_string() });
            }
            serde_json::Value::Array(arr) => {
                if arr.len() != 3 {
                    return Err(format!("malformed condition array (expected 3 elements): {:?}", arr));
                }
                let op = arr[0].as_str().ok_or("condition operator must be a string")?;
                if op.eq_ignore_ascii_case("content-length-range") {
                    let min = arr[1].as_i64().ok_or("content-length-range min must be an integer")?;
                    let max = arr[2].as_i64().ok_or("content-length-range max must be an integer")?;
                    if min < 0 || max < min {
                        return Err(format!("invalid content-length-range bounds [{min}, {max}]"));
                    }
                    out.push(Condition::ContentLengthRange { min, max });
                } else if op.eq_ignore_ascii_case("eq") || op.eq_ignore_ascii_case("starts-with") {
                    let field = arr[1].as_str().ok_or("condition field must be a string")?;
                    let field = field.strip_prefix('$').unwrap_or(field).to_lowercase();
                    let value = arr[2].as_str().ok_or("condition value must be a string")?;
                    if op.eq_ignore_ascii_case("eq") {
                        out.push(Condition::Eq { field, value: value.to_string() });
                    } else {
                        out.push(Condition::StartsWith { field, prefix: value.to_string() });
                    }
                } else {
                    return Err(format!("unknown condition operator: {op}"));
                }
            }
            _ => return Err("condition must be an object or array".to_string()),
        }
    }
    Ok(out)
}

/// Fields the policy is allowed to (and, if submitted, must) govern.
/// Anything outside this set is passed through unchecked -- confirmed by
/// `test_post_object_ignored_header`, which sends an arbitrary
/// `x-ignore-foo` field with no matching condition and still succeeds.
fn field_is_policy_checked(name: &str) -> bool {
    matches!(
        name,
        "bucket" | "key" | "acl" | "content-type" | "content-disposition" | "content-encoding"
            | "cache-control" | "expires" | "success_action_redirect" | "success_action_status"
            | "tagging"
    ) || name.starts_with("x-amz-")
}

fn condition_matches(cond: &Condition, actual: Option<&str>) -> bool {
    match cond {
        Condition::Eq { value, .. } => actual == Some(value.as_str()),
        Condition::StartsWith { prefix, .. } => actual.is_some_and(|a| a.starts_with(prefix.as_str())),
        Condition::ContentLengthRange { .. } => true, // checked separately, against the file's real size
    }
}

fn condition_field(cond: &Condition) -> Option<&str> {
    match cond {
        Condition::Eq { field, .. } => Some(field),
        Condition::StartsWith { field, .. } => Some(field),
        Condition::ContentLengthRange { .. } => None,
    }
}

/// Admin-bypass-only secret resolution, deliberately not reusing
/// `auth.rs`'s Console-backed credential cache: every Ceph test this
/// unlocks signs with `get_main_aws_access_key()`/`get_main_aws_secret_key()`,
/// which in this project's own test setup are the
/// `WARPDRIVE_ADMIN_ACCESS_KEY`/`WARPDRIVE_ADMIN_SECRET_KEY` admin-bypass
/// pair -- the same credential resolution every other S3-compat batch this
/// project has shipped relies on for testing. A real Console-routed POST
/// Object path is a real gap, not silently assumed away; it just isn't
/// exercised by anything in scope here yet.
fn resolve_secret_key(access_key: &str) -> Option<(String, String)> {
    let admin_access_key = std::env::var("WARPDRIVE_ADMIN_ACCESS_KEY").ok()?;
    let admin_secret_key = std::env::var("WARPDRIVE_ADMIN_SECRET_KEY").ok()?;
    if access_key == admin_access_key {
        Some(("admin".to_string(), admin_secret_key))
    } else {
        None
    }
}

/// Form fields are matched by name, not by a single known header -- scan
/// for whichever `x-amz-checksum-{suffix}` the client actually sent.
fn find_checksum_field(f: &HashMap<String, String>) -> Option<(crate::s3::handlers::checksum::ChecksumAlgorithm, String)> {
    use crate::s3::handlers::checksum::ChecksumAlgorithm;
    for algo in [ChecksumAlgorithm::Sha256, ChecksumAlgorithm::Sha1, ChecksumAlgorithm::Crc32, ChecksumAlgorithm::Crc32c, ChecksumAlgorithm::Crc64Nvme] {
        let field = format!("x-amz-checksum-{}", algo.header_suffix());
        if let Some(value) = f.get(&field) {
            return Some((algo, value.clone()));
        }
    }
    None
}

fn post_object_error(status: StatusCode, code: &str, message: &str, bucket: &str) -> HttpResponse {
    s3_error(status, code, message, &format!("/{}", bucket))
}

/// `POST /{bucket}` is shared with multi-object delete (`?delete`, XML
/// body) in real S3 -- route on that query param first, same as
/// `s3_multipart_router` already disambiguates several operations sharing
/// one POST route on `/{bucket}/{key}`. A `Multipart` extractor can't be
/// declared directly as this handler's own parameter (it would try to
/// parse non-multipart delete-request bodies as multipart and fail before
/// this function's body ever runs), so the raw `web::Payload` is taken
/// here and handed to `Multipart::new` only once this really is a form
/// upload.
pub async fn s3_post_bucket_handler(
    path: web::Path<String>,
    query: web::Query<HashMap<String, String>>,
    payload: web::Payload,
    req: HttpRequest,
) -> Result<HttpResponse, Error> {
    if query.contains_key("delete") {
        return super::listing::s3_delete_objects_handler(path, query, payload, req).await;
    }
    let bucket = path.into_inner();
    let multipart = Multipart::new(req.headers(), payload);
    s3_post_object_handler_inner(bucket, multipart).await
}

async fn s3_post_object_handler_inner(bucket: String, payload: Multipart) -> Result<HttpResponse, Error> {
    let form = parse_multipart_form(payload).await?;
    let f = &form.fields;

    // "no key specified" is a distinct, more basic failure than a missing
    // *condition* for key -- the form didn't even try to supply one.
    let mut key = match f.get("key") {
        Some(k) => k.clone(),
        None => return Ok(post_object_error(StatusCode::BAD_REQUEST, "InvalidArgument", "no key specified", &bucket)),
    };
    if key == "${filename}" {
        key = form.file_name.clone().unwrap_or_default();
    }

    let policy_b64 = f.get("policy");

    // Single admin-user system throughout this project (see auth.rs):
    // there's one real tenant, so looking up the bucket's ACL under that
    // identity is safe regardless of whether this turns out to be an
    // anonymous or a signed request.
    let bucket_owner_db = MetadataService::new(crate::s3::auth::ADMIN_USER_ID)?;
    let bucket_grants = match bucket_owner_db.get_bucket_acl(&bucket) {
        Ok(Some((owner_id, grants_json))) => (owner_id, super::acl::grants_from_json(&grants_json)),
        _ => (crate::s3::auth::ADMIN_USER_ID.to_string(), Vec::new()),
    };

    let user_id = if let Some(policy_b64) = policy_b64 {
        let access_key = match f.get("awsaccesskeyid") {
            Some(a) => a.clone(),
            None => return Ok(post_object_error(StatusCode::BAD_REQUEST, "InvalidArgument", "missing AWSAccessKeyId", &bucket)),
        };
        let signature = match f.get("signature") {
            Some(s) => s.clone(),
            None => return Ok(post_object_error(StatusCode::BAD_REQUEST, "InvalidArgument", "missing signature", &bucket)),
        };
        let (user_id, secret_key) = match resolve_secret_key(&access_key) {
            Some(pair) => pair,
            None => return Ok(post_object_error(StatusCode::FORBIDDEN, "InvalidAccessKeyId", "The AWS Access Key Id you provided does not exist in our records.", &bucket)),
        };

        // Signature: base64(HMAC-SHA1(secret_key, policy_base64_string)) --
        // over the base64 *text* of the policy, not the decoded JSON bytes.
        let mut mac = HmacSha1::new_from_slice(secret_key.as_bytes())
            .map_err(|_| actix_web::error::ErrorInternalServerError("HMAC init"))?;
        mac.update(policy_b64.as_bytes());
        let expected_sig_bytes = mac.finalize().into_bytes();
        let expected_sig = base64::engine::general_purpose::STANDARD.encode(expected_sig_bytes);
        if expected_sig != signature {
            return Ok(post_object_error(StatusCode::FORBIDDEN, "SignatureDoesNotMatch", "The request signature we calculated does not match the signature you provided.", &bucket));
        }

        let policy_json_bytes = base64::engine::general_purpose::STANDARD.decode(policy_b64.as_bytes())
            .map_err(|_| actix_web::error::ErrorBadRequest("policy is not valid base64"))?;
        let policy: serde_json::Value = match serde_json::from_slice(&policy_json_bytes) {
            Ok(v) => v,
            Err(_) => return Ok(post_object_error(StatusCode::BAD_REQUEST, "InvalidArgument", "policy is not valid JSON", &bucket)),
        };
        let policy_obj = policy.as_object()
            .ok_or_else(|| actix_web::error::ErrorBadRequest("policy document must be a JSON object"))?;

        // Top-level keys are case-sensitive: `test_post_object_condition_is_case_sensitive`
        // sends "CONDITIONS", `test_post_object_expires_is_case_sensitive` sends
        // "EXPIRATION" -- neither counts as the real field.
        let expiration_str = match policy_obj.get("expiration").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return Ok(post_object_error(StatusCode::BAD_REQUEST, "InvalidArgument", "policy document missing \"expiration\"", &bucket)),
        };
        // Strict format only -- `test_post_object_invalid_date_format` sends
        // Python's default `str(datetime)` rendering ("YYYY-MM-DD HH:MM:SS+00:00"),
        // which must NOT parse here.
        let expiration = chrono::NaiveDateTime::parse_from_str(expiration_str, "%Y-%m-%dT%H:%M:%SZ")
            .map(|dt| dt.and_utc());
        let expiration = match expiration {
            Ok(dt) => dt,
            Err(_) => return Ok(post_object_error(StatusCode::BAD_REQUEST, "InvalidArgument", "policy \"expiration\" is not a valid ISO-8601 UTC timestamp", &bucket)),
        };
        if expiration < chrono::Utc::now() {
            return Ok(post_object_error(StatusCode::FORBIDDEN, "ExpiredToken", "Policy expired.", &bucket));
        }

        let conditions_arr = match policy_obj.get("conditions").and_then(|v| v.as_array()) {
            Some(a) => a,
            None => return Ok(post_object_error(StatusCode::BAD_REQUEST, "InvalidArgument", "policy document missing \"conditions\" array", &bucket)),
        };
        let conditions = match parse_conditions(conditions_arr) {
            Ok(c) => c,
            Err(msg) => return Ok(post_object_error(StatusCode::BAD_REQUEST, "InvalidArgument", &format!("malformed policy conditions: {msg}"), &bucket)),
        };

        // The "bucket" condition is implicit authorization for *which*
        // bucket this policy may be used against -- checked against the
        // real URL-path bucket, never a form-supplied "bucket" field (a
        // client could put anything there; `test_post_object_wrong_bucket`
        // relies on exactly this not being trusted).
        let bucket_cond = conditions.iter().find(|c| condition_field(c) == Some("bucket"));
        match bucket_cond {
            None => return Ok(post_object_error(StatusCode::FORBIDDEN, "AccessDenied", "Policy is missing a bucket condition.", &bucket)),
            Some(c) if !condition_matches(c, Some(bucket.as_str())) => {
                return Ok(post_object_error(StatusCode::FORBIDDEN, "AccessDenied", "Policy does not authorize this bucket.", &bucket));
            }
            _ => {}
        }

        // Every *condition* in the policy must be satisfied by the actual
        // submission -- but the reverse isn't required: a submitted field
        // with no matching condition is simply not policy-checked, not an
        // error. Confirmed by two real tests: `test_post_object_ignored_header`
        // (an arbitrary `x-ignore-foo` field, no condition, still 204) and
        // `test_post_object_upload_checksum` (`x-amz-checksum-sha256` sent
        // with no corresponding condition in the policy, still 204) --
        // an earlier version of this handler rejected the second case by
        // over-applying the "submitted field needs a condition" rule to
        // every `x-amz-*` field, which real S3 does not do.
        let mut submitted: HashMap<&str, &str> = f.iter()
            .filter(|(k, _)| field_is_policy_checked(k))
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        submitted.insert("key", key.as_str());

        for cond in &conditions {
            if let Some(field) = condition_field(cond) {
                if field == "bucket" { continue; }
                let actual = submitted.get(field).copied();
                if !condition_matches(cond, actual) {
                    return Ok(post_object_error(StatusCode::FORBIDDEN, "AccessDenied", &format!("Policy Condition failed: [{field}]"), &bucket));
                }
            }
        }

        let file_len = form.file_bytes.len() as i64;
        for cond in &conditions {
            if let Condition::ContentLengthRange { min, max } = cond {
                if file_len < *min || file_len > *max {
                    return Ok(post_object_error(
                        StatusCode::BAD_REQUEST, "EntityTooLarge",
                        &format!("file size {file_len} is outside the policy's content-length-range [{min}, {max}]"),
                        &bucket,
                    ));
                }
            }
        }

        user_id
    } else {
        // No policy at all: an anonymous POST, only valid against a
        // bucket ACL that grants public-write (`test_post_object_anonymous_request`
        // creates its bucket with `ACL='public-read-write'` specifically
        // to exercise this).
        if !super::acl::is_publicly_writable(&bucket_grants.1) {
            return Ok(post_object_error(
                StatusCode::FORBIDDEN, "AccessDenied",
                "Anonymous POST Object requires a bucket ACL granting public write access.",
                &bucket,
            ));
        }
        bucket_grants.0.clone()
    };

    // An `x-amz-checksum-*` field is policy-pass-through (see the
    // field_is_policy_checked doc above), not an authorization input --
    // but once present, it still has to be cryptographically checked
    // against the real uploaded bytes, same as the regular PUT path does.
    // `test_post_object_upload_checksum`'s second half proves this: a
    // garbage `x-amz-checksum-sha256` value must be rejected with 400,
    // not silently accepted as an opaque string nobody ever verifies.
    let checksum_field = find_checksum_field(f);
    if let Some((ref algo, ref expected)) = checksum_field {
        if !crate::s3::handlers::checksum::verify_checksum(algo, &form.file_bytes, expected) {
            return Ok(post_object_error(
                StatusCode::BAD_REQUEST, "BadDigest",
                "The Content-MD5 or checksum you specified did not match what we received.",
                &bucket,
            ));
        }
    }

    // --- Policy satisfied: store the object, same primitives the regular
    // PUT handler uses (StorageConfig store + MetadataService), not the
    // PUT handler itself, to stay independent of its in-flight ACL-port edits.
    let db = MetadataService::new(&user_id)?;
    if let Err(resp) = require_bucket(&db, &bucket) { return Ok(resp); }

    // web::block: `Storage::write` now does a real (group-committed) fsync
    // -- see `storage::group_commit`'s module doc -- so this has to go
    // through `web::block` the same way `object.rs`'s PUT handler does;
    // calling it directly here would block this actix worker's whole
    // event loop for the fsync's duration.
    let store = StorageConfig::from_env().create_store();
    let user_id_owned = user_id.clone();
    let bucket_owned = bucket.clone();
    let file_bytes = form.file_bytes.clone();
    let (offset, size) = web::block(move || {
        store.write(&user_id_owned, &bucket_owned, &file_bytes).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| actix_web::error::ErrorInternalServerError(format!("blocking task failed: {e}")))?
    .map_err(actix_web::error::ErrorInternalServerError)?;

    let etag = md5_etag(&form.file_bytes);
    let content_type = f.get("content-type").cloned();
    let user_metadata: HashMap<String, String> = f.iter()
        .filter_map(|(k, v)| k.strip_prefix("x-amz-meta-").map(|mk| (mk.to_string(), v.clone())))
        .collect();

    let mut metadata = Metadata::from_offset_size_list(vec![(offset, size)]);
    metadata.etag = Some(etag.clone());
    metadata.size = size;
    metadata.content_type = content_type;
    metadata.last_modified = Some(last_modified_now());
    if let Some((algo, value)) = checksum_field {
        metadata.checksum_algorithm = Some(algo.as_str().to_string());
        metadata.checksum_value = Some(value);
    }
    metadata.user_metadata = user_metadata;

    let (version_id, old_extents) = db.put_object_full(&bucket, &key, metadata)?;
    if !old_extents.is_empty() {
        db.queue_deletion(&bucket, &key, &old_extents).ok();
    }
    let effective_vid = version_id.as_deref().unwrap_or("");

    // "acl" form field -> canned ACL on the new object, same semantics as
    // PUT's `x-amz-acl` header (just sourced from a form field instead of
    // a header, since there is no header on a POST form upload).
    if let Some(acl_value) = f.get("acl") {
        let owner_display = if user_id == crate::s3::auth::ADMIN_USER_ID {
            crate::s3::auth::admin_display_name()
        } else {
            user_id.clone()
        };
        match super::acl::expand_canned(acl_value, &user_id, &owner_display) {
            Ok(grants) => db.set_object_acl(&bucket, &key, effective_vid, &user_id, &super::acl::grants_to_json(&grants))?,
            Err(resp) => return Ok(resp),
        }
    }

    if let Some(tagging_xml) = f.get("tagging") {
        let tags = parse_tag_xml(tagging_xml);
        let resource = format!("/{}/{}", bucket, key);
        if let Err(resp) = validate_tags(&tags, &resource) { return Ok(resp); }
        db.set_object_tags(&bucket, &key, &tags)?;
    }

    let success_status = f.get("success_action_status").and_then(|s| s.parse::<u16>().ok());
    if let Some(redirect) = f.get("success_action_redirect") {
        let location = format!("{redirect}?bucket={bucket}&key={key}&etag=%22{}%22", etag.trim_matches('"'));
        return Ok(HttpResponse::Found().insert_header(("Location", location.clone())).insert_header(("ETag", etag)).body(location));
    }
    match success_status {
        Some(201) => {
            let xml = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
                 <PostResponse><Location>/{bucket}/{key}</Location><Bucket>{bucket}</Bucket>\
                 <Key>{key}</Key><ETag>{etag}</ETag></PostResponse>"
            );
            Ok(HttpResponse::Created().content_type("application/xml").body(xml))
        }
        _ => Ok(HttpResponse::NoContent().insert_header(("ETag", etag)).finish()),
    }
}
