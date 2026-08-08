// ACL: canned ACLs, header-based grants, arbitrary AccessControlPolicy XML bodies,
// Block Public Access, and ACL-derived bucket policy status.
//
// Design note: permission checks below only ever gate *anonymous* requests (see
// `authenticate_s3_request_allow_anonymous` in auth.rs). An authenticated request is
// always the resource owner in today's single-admin setup, so it always gets full
// access — matching the RFC's own scoping ("admin always has full access to
// everything they own"). Grant-list storage never hardcodes "admin": it's built from
// whatever `auth_result.user_id`/`owner_display_name` auth resolves to, so real
// per-principal grant evaluation is a pure addition once Vitality Console (UAM)
// resolves more than one identity — nothing here needs to change for that.
use actix_web::{HttpRequest, HttpResponse, Error, http::StatusCode};
use serde::{Deserialize, Serialize};

use super::common::*;

pub(super) const URI_ALL_USERS: &str = "http://acs.amazonaws.com/groups/global/AllUsers";
pub(super) const URI_AUTHENTICATED_USERS: &str = "http://acs.amazonaws.com/groups/global/AuthenticatedUsers";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type")]
pub(super) enum Grantee {
    CanonicalUser { id: String, display_name: Option<String> },
    Group { uri: String },
    Email { email: String },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub(super) enum Permission {
    Read,
    Write,
    ReadAcp,
    WriteAcp,
    FullControl,
}

impl Permission {
    fn as_str(&self) -> &'static str {
        match self {
            Permission::Read => "READ",
            Permission::Write => "WRITE",
            Permission::ReadAcp => "READ_ACP",
            Permission::WriteAcp => "WRITE_ACP",
            Permission::FullControl => "FULL_CONTROL",
        }
    }
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "READ" => Some(Permission::Read),
            "WRITE" => Some(Permission::Write),
            "READ_ACP" => Some(Permission::ReadAcp),
            "WRITE_ACP" => Some(Permission::WriteAcp),
            "FULL_CONTROL" => Some(Permission::FullControl),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct AclGrant {
    pub grantee: Grantee,
    pub permission: Permission,
}

fn owner_grant(owner_id: &str, owner_display: &str) -> AclGrant {
    AclGrant {
        grantee: Grantee::CanonicalUser {
            id: owner_id.to_string(),
            display_name: Some(owner_display.to_string()),
        },
        permission: Permission::FullControl,
    }
}

/// Expand a canned ACL keyword into its grant list. `bucket-owner-read` /
/// `bucket-owner-full-control` only differ from `private` when the object owner isn't
/// the bucket owner — unreachable without a second real identity, so they resolve to
/// owner-FULL_CONTROL for now (the tests that would distinguish them are deferred).
pub(super) fn expand_canned(canned: &str, owner_id: &str, owner_display: &str) -> Result<Vec<AclGrant>, HttpResponse> {
    let owner = owner_grant(owner_id, owner_display);
    match canned {
        "private" | "bucket-owner-read" | "bucket-owner-full-control" => Ok(vec![owner]),
        "public-read" => Ok(vec![
            AclGrant { grantee: Grantee::Group { uri: URI_ALL_USERS.to_string() }, permission: Permission::Read },
            owner,
        ]),
        "public-read-write" => Ok(vec![
            AclGrant { grantee: Grantee::Group { uri: URI_ALL_USERS.to_string() }, permission: Permission::Read },
            AclGrant { grantee: Grantee::Group { uri: URI_ALL_USERS.to_string() }, permission: Permission::Write },
            owner,
        ]),
        "authenticated-read" => Ok(vec![
            AclGrant { grantee: Grantee::Group { uri: URI_AUTHENTICATED_USERS.to_string() }, permission: Permission::Read },
            owner,
        ]),
        _ => Err(s3_error(StatusCode::BAD_REQUEST, "InvalidArgument",
                          "Invalid canned ACL", canned)),
    }
}

fn parse_grantee_spec(spec: &str) -> Option<Grantee> {
    let (k, v) = spec.split_once('=')?;
    let v = v.trim().trim_matches('"').to_string();
    match k.trim() {
        "id" => Some(Grantee::CanonicalUser { id: v, display_name: None }),
        "uri" => Some(Grantee::Group { uri: v }),
        "emailAddress" => Some(Grantee::Email { email: v }),
        _ => None,
    }
}

/// Parse `x-amz-grant-{read,write,read-acp,write-acp,full-control}` headers. Each
/// value is a comma-separated list of `id=...` / `uri=...` / `emailAddress=...`
/// grantee specs (AWS spec). Returns `None` when no grant headers are present at all
/// (caller falls back to canned ACL / XML body / default-private). When present,
/// header grants *replace* the grant list entirely — no implicit owner grant is added,
/// confirmed by the Ceph tests (`test_object_header_acl_grants` etc. expect exactly
/// the granted permissions, no separate owner entry).
pub(super) fn grants_from_headers(req: &HttpRequest) -> Option<Result<Vec<AclGrant>, HttpResponse>> {
    let header_perms = [
        ("x-amz-grant-read", Permission::Read),
        ("x-amz-grant-write", Permission::Write),
        ("x-amz-grant-read-acp", Permission::ReadAcp),
        ("x-amz-grant-write-acp", Permission::WriteAcp),
        ("x-amz-grant-full-control", Permission::FullControl),
    ];
    let mut any = false;
    let mut grants = Vec::new();
    for (header, perm) in header_perms {
        if let Some(val) = req.headers().get(header).and_then(|v| v.to_str().ok()) {
            any = true;
            for spec in val.split(',') {
                let spec = spec.trim();
                if spec.is_empty() { continue; }
                match parse_grantee_spec(spec) {
                    Some(Grantee::Email { .. }) => {
                        // No email→canonical-ID directory in static-admin mode.
                        return Some(Err(s3_error(StatusCode::BAD_REQUEST, "InvalidArgument",
                                                 "The specified email address does not resolve to an account",
                                                 spec)));
                    }
                    Some(grantee) => grants.push(AclGrant { grantee, permission: perm }),
                    None => return Some(Err(s3_error(StatusCode::BAD_REQUEST, "InvalidArgument",
                                                       "Invalid grantee specification", spec))),
                }
            }
        }
    }
    if !any { return None; }
    Some(Ok(grants))
}

/// Find `<tag ...>...</tag>` (attributes on the opening tag, unlike `extract_xml_tag`
/// which only matches a bare `<tag>`) and return the whole block including both tags.
fn extract_tag_block(src: &str, tag: &str) -> Option<String> {
    let open_marker = format!("<{}", tag);
    let open_start = src.find(&open_marker)?;
    let after = &src[open_start..];
    let close_tag = format!("</{}>", tag);
    let close_start = after.find(&close_tag)?;
    Some(after[..close_start + close_tag.len()].to_string())
}

fn parse_grantee_block(full_block: &str) -> Option<Grantee> {
    let open_end = full_block.find('>')?;
    let open_tag = &full_block[..open_end];
    let inner = &full_block[open_end + 1..full_block.rfind("</Grantee>")?];
    if open_tag.contains("\"CanonicalUser\"") {
        let id = extract_xml_tag(inner, "ID")?;
        let display_name = extract_xml_tag(inner, "DisplayName");
        Some(Grantee::CanonicalUser { id, display_name })
    } else if open_tag.contains("\"Group\"") {
        let uri = extract_xml_tag(inner, "URI")?;
        Some(Grantee::Group { uri })
    } else if open_tag.contains("AmazonCustomerByEmail") {
        let email = extract_xml_tag(inner, "EmailAddress")?;
        Some(Grantee::Email { email })
    } else {
        None
    }
}

/// Parse an explicit `AccessControlPolicy` XML body (`PUT ?acl` with a body instead of
/// canned/header grants). An empty `<AccessControlList/>` parses to an empty grant
/// list, stored verbatim (`test_bucket_acl_revoke_all`).
pub(super) fn parse_acl_xml(body: &str) -> Result<Vec<AclGrant>, HttpResponse> {
    let acl_block = extract_xml_tag(body, "AccessControlList").unwrap_or_default();
    let mut grants = Vec::new();
    for block in extract_all_xml_tags(&acl_block, "Grant") {
        let permission_str = extract_xml_tag(&block, "Permission").unwrap_or_default();
        let permission = Permission::from_str(&permission_str)
            .ok_or_else(|| s3_error(StatusCode::BAD_REQUEST, "MalformedACLError",
                                    "Invalid permission in ACL", &permission_str))?;
        let grantee_block = extract_tag_block(&block, "Grantee")
            .ok_or_else(|| s3_error(StatusCode::BAD_REQUEST, "MalformedACLError",
                                    "Missing Grantee in ACL grant", ""))?;
        let grantee = parse_grantee_block(&grantee_block)
            .ok_or_else(|| s3_error(StatusCode::BAD_REQUEST, "InvalidArgument",
                                    "Invalid grantee in ACL", ""))?;
        grants.push(AclGrant { grantee, permission });
    }
    Ok(grants)
}

/// Reorder so the owner's own CanonicalUser grant sorts last. Every achievable Ceph
/// ACL test expects group/public grants before the owner's FULL_CONTROL entry
/// (`test_bucket_acl_canned`, `test_put_bucket_acl_grant_group_read`, etc.) — `boto3`'s
/// `check_grants` test helper only tolerates arbitrary ordering when every grant has a
/// DisplayName, which isn't true here (group grants don't), so exact positional order
/// matters.
fn normalize_grant_order(grants: Vec<AclGrant>, owner_id: &str) -> Vec<AclGrant> {
    let (owner, rest): (Vec<_>, Vec<_>) = grants.into_iter().partition(|g| {
        matches!(&g.grantee, Grantee::CanonicalUser { id, .. } if id == owner_id)
            && g.permission == Permission::FullControl
    });
    let mut out = rest;
    out.extend(owner);
    out
}

fn grants_to_xml(owner_id: &str, owner_display: &str, grants: &[AclGrant]) -> String {
    let mut grants_xml = String::new();
    for g in grants {
        let grantee_xml = match &g.grantee {
            Grantee::CanonicalUser { id, display_name } => format!(
                "<Grantee xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" xsi:type=\"CanonicalUser\">\
                 <ID>{}</ID><DisplayName>{}</DisplayName></Grantee>",
                xml_escape(id), xml_escape(display_name.as_deref().unwrap_or(id))
            ),
            Grantee::Group { uri } => format!(
                "<Grantee xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" xsi:type=\"Group\">\
                 <URI>{}</URI></Grantee>",
                xml_escape(uri)
            ),
            Grantee::Email { email } => format!(
                "<Grantee xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" xsi:type=\"AmazonCustomerByEmail\">\
                 <EmailAddress>{}</EmailAddress></Grantee>",
                xml_escape(email)
            ),
        };
        grants_xml.push_str(&format!(
            "<Grant>{}<Permission>{}</Permission></Grant>",
            grantee_xml, g.permission.as_str()
        ));
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <AccessControlPolicy xmlns=\"{s3}\">\
           <Owner><ID>{oid}</ID><DisplayName>{odn}</DisplayName></Owner>\
           <AccessControlList>{grants}</AccessControlList>\
         </AccessControlPolicy>",
        s3 = S3_XMLNS, oid = xml_escape(owner_id), odn = xml_escape(owner_display), grants = grants_xml,
    )
}

pub(super) fn grants_to_json(grants: &[AclGrant]) -> String {
    serde_json::to_string(grants).unwrap_or_else(|_| "[]".to_string())
}

pub(super) fn grants_from_json(json: &str) -> Vec<AclGrant> {
    serde_json::from_str(json).unwrap_or_default()
}

/// Precedence: `x-amz-grant-*` headers > `x-amz-acl` canned header > XML body >
/// default-private. Real S3 rejects both a canned header and grant headers together.
pub(super) fn resolve_effective_grants(
    req: &HttpRequest,
    body: Option<&str>,
    owner_id: &str,
    owner_display: &str,
) -> Result<Vec<AclGrant>, HttpResponse> {
    let canned = req.headers().get("x-amz-acl").and_then(|v| v.to_str().ok());
    let header_grants = grants_from_headers(req);

    let grants = match (canned, header_grants) {
        (Some(_), Some(_)) => {
            return Err(s3_error(StatusCode::BAD_REQUEST, "InvalidArgument",
                                "Specify only one of x-amz-acl or x-amz-grant-*", ""));
        }
        (_, Some(hg)) => hg?,
        (Some(c), None) => expand_canned(c, owner_id, owner_display)?,
        (None, None) => match body.filter(|b| !b.trim().is_empty()) {
            Some(b) => parse_acl_xml(b)?,
            None => expand_canned("private", owner_id, owner_display)?,
        },
    };
    Ok(normalize_grant_order(grants, owner_id))
}

pub(super) fn grants_contain_group(grants: &[AclGrant], uri: &str, perms: &[Permission]) -> bool {
    grants.iter().any(|g| {
        matches!(&g.grantee, Grantee::Group { uri: u } if u == uri)
            && perms.contains(&g.permission)
    })
}

const READABLE: [Permission; 2] = [Permission::Read, Permission::FullControl];
const WRITABLE: [Permission; 2] = [Permission::Write, Permission::FullControl];

pub(super) fn is_publicly_readable(grants: &[AclGrant]) -> bool {
    grants_contain_group(grants, URI_ALL_USERS, &READABLE)
}

pub(super) fn is_publicly_writable(grants: &[AclGrant]) -> bool {
    grants_contain_group(grants, URI_ALL_USERS, &WRITABLE)
}

pub(super) fn is_authenticated_readable(grants: &[AclGrant]) -> bool {
    grants_contain_group(grants, URI_AUTHENTICATED_USERS, &READABLE)
}

/// True if any grant hands anything to AllUsers/AuthenticatedUsers, regardless of
/// permission — used for Block Public Access's `block_public_acls` rejection (real S3
/// rejects `authenticated-read` too, not just public-read/public-read-write).
pub(super) fn grants_contain_public(grants: &[AclGrant]) -> bool {
    grants.iter().any(|g| matches!(&g.grantee, Grantee::Group { uri }
        if uri == URI_ALL_USERS || uri == URI_AUTHENTICATED_USERS))
}

/// Reject storing a public grant when the bucket's Block Public Access has
/// `block_public_acls` set (`test_block_public_put_bucket_acls`,
/// `test_block_public_object_canned_acls`).
fn check_block_public_acls(db: &crate::service::metadata_service::MetadataService, bucket: &str, grants: &[AclGrant]) -> Result<(), HttpResponse> {
    if let Ok(Some((block_public_acls, _, _, _))) = db.get_public_access_block(bucket) {
        if block_public_acls && grants_contain_public(grants) {
            return Err(s3_error(StatusCode::FORBIDDEN, "AccessDenied",
                                "Public access blocked for this bucket", bucket));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Bucket ACL  GET/PUT /s3/{bucket}?acl
// ---------------------------------------------------------------------------

pub(super) async fn s3_get_bucket_acl_inner(bucket: &str, req: &HttpRequest) -> Result<HttpResponse, Error> {
    use crate::s3::auth::authenticate_s3_request;
    use crate::service::metadata_service::MetadataService;

    let auth_result = authenticate_s3_request(req).await?;
    let db = MetadataService::new(&auth_result.user_id)?;
    if let Err(resp) = require_bucket(&db, bucket) { return Ok(resp); }

    let (owner_id, grants) = match db.get_bucket_acl(bucket) {
        Ok(Some((owner_id, grants_json))) => (owner_id, grants_from_json(&grants_json)),
        _ => (auth_result.user_id.clone(), expand_canned("private", &auth_result.user_id, &auth_result.owner_display_name).unwrap_or_default()),
    };
    let owner_display = if owner_id == auth_result.user_id { auth_result.owner_display_name.clone() } else { owner_id.clone() };
    let xml = grants_to_xml(&owner_id, &owner_display, &grants);
    Ok(HttpResponse::Ok().content_type("application/xml").body(xml))
}

pub(super) async fn s3_put_bucket_acl_inner(bucket: &str, body: &[u8], req: &HttpRequest) -> Result<HttpResponse, Error> {
    use crate::s3::auth::authenticate_s3_request;
    use crate::service::metadata_service::MetadataService;

    let auth_result = authenticate_s3_request(req).await?;
    let db = MetadataService::new(&auth_result.user_id)?;
    if let Err(resp) = require_bucket(&db, bucket) { return Ok(resp); }

    let body_str = String::from_utf8_lossy(body);
    let grants = match resolve_effective_grants(req, Some(&body_str), &auth_result.user_id, &auth_result.owner_display_name) {
        Ok(g) => g,
        Err(resp) => return Ok(resp),
    };
    if let Err(resp) = check_block_public_acls(&db, bucket, &grants) { return Ok(resp); }

    db.set_bucket_acl(bucket, &auth_result.user_id, &grants_to_json(&grants))?;
    Ok(HttpResponse::Ok().insert_header(("Content-Length", "0")).body(""))
}

// ---------------------------------------------------------------------------
// Object ACL  GET/PUT /s3/{bucket}/{key}?acl
// ---------------------------------------------------------------------------

pub(super) async fn s3_get_object_acl_inner(bucket: &str, key: &str, req: &HttpRequest) -> Result<HttpResponse, Error> {
    use crate::s3::auth::authenticate_s3_request;
    use crate::service::metadata_service::MetadataService;

    let auth_result = authenticate_s3_request(req).await?;
    let db = MetadataService::new(&auth_result.user_id)?;
    if let Err(resp) = require_bucket(&db, bucket) { return Ok(resp); }
    if !db.check_key(bucket, key)? {
        return Ok(s3_error(StatusCode::NOT_FOUND, "NoSuchKey",
                           "The specified key does not exist.", &format!("/{}/{}", bucket, key)));
    }

    let (owner_id, grants) = match db.get_object_acl(bucket, key, "") {
        Ok(Some((owner_id, grants_json))) => (owner_id, grants_from_json(&grants_json)),
        _ => (auth_result.user_id.clone(), expand_canned("private", &auth_result.user_id, &auth_result.owner_display_name).unwrap_or_default()),
    };
    let owner_display = if owner_id == auth_result.user_id { auth_result.owner_display_name.clone() } else { owner_id.clone() };
    let xml = grants_to_xml(&owner_id, &owner_display, &grants);
    Ok(HttpResponse::Ok().content_type("application/xml").body(xml))
}

pub(super) async fn s3_put_object_acl_inner(bucket: &str, key: &str, body: &[u8], req: &HttpRequest) -> Result<HttpResponse, Error> {
    use crate::s3::auth::authenticate_s3_request;
    use crate::service::metadata_service::MetadataService;

    let auth_result = authenticate_s3_request(req).await?;
    let db = MetadataService::new(&auth_result.user_id)?;
    if let Err(resp) = require_bucket(&db, bucket) { return Ok(resp); }
    if !db.check_key(bucket, key)? {
        return Ok(s3_error(StatusCode::NOT_FOUND, "NoSuchKey",
                           "The specified key does not exist.", &format!("/{}/{}", bucket, key)));
    }

    let body_str = String::from_utf8_lossy(body);
    let grants = match resolve_effective_grants(req, Some(&body_str), &auth_result.user_id, &auth_result.owner_display_name) {
        Ok(g) => g,
        Err(resp) => return Ok(resp),
    };
    if let Err(resp) = check_block_public_acls(&db, bucket, &grants) { return Ok(resp); }

    db.set_object_acl(bucket, key, "", &auth_result.user_id, &grants_to_json(&grants))?;
    Ok(HttpResponse::Ok().insert_header(("Content-Length", "0")).body(""))
}

// ---------------------------------------------------------------------------
// Block Public Access  GET/PUT/DELETE /s3/{bucket}?publicAccessBlock
// ---------------------------------------------------------------------------

pub(super) async fn s3_get_public_access_block_inner(bucket: &str, req: &HttpRequest) -> Result<HttpResponse, Error> {
    use crate::s3::auth::authenticate_s3_request;
    use crate::service::metadata_service::MetadataService;

    let auth_result = authenticate_s3_request(req).await?;
    let db = MetadataService::new(&auth_result.user_id)?;
    if let Err(resp) = require_bucket(&db, bucket) { return Ok(resp); }

    match db.get_public_access_block(bucket) {
        Ok(Some((a, i, p, r))) => {
            let b = |v: bool| if v { "true" } else { "false" };
            let xml = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
                 <PublicAccessBlockConfiguration xmlns=\"{s3}\">\
                   <BlockPublicAcls>{a}</BlockPublicAcls>\
                   <IgnorePublicAcls>{i}</IgnorePublicAcls>\
                   <BlockPublicPolicy>{p}</BlockPublicPolicy>\
                   <RestrictPublicBuckets>{r}</RestrictPublicBuckets>\
                 </PublicAccessBlockConfiguration>",
                s3 = S3_XMLNS, a = b(a), i = b(i), p = b(p), r = b(r),
            );
            Ok(HttpResponse::Ok().content_type("application/xml").body(xml))
        }
        _ => Ok(s3_error(StatusCode::NOT_FOUND, "NoSuchPublicAccessBlockConfiguration",
                         "The public access block configuration was not found", bucket)),
    }
}

pub(super) async fn s3_put_public_access_block_inner(bucket: &str, body: &[u8], req: &HttpRequest) -> Result<HttpResponse, Error> {
    use crate::s3::auth::authenticate_s3_request;
    use crate::service::metadata_service::MetadataService;

    let auth_result = authenticate_s3_request(req).await?;
    let db = MetadataService::new(&auth_result.user_id)?;
    if let Err(resp) = require_bucket(&db, bucket) { return Ok(resp); }

    let body_str = String::from_utf8_lossy(body);
    let flag = |tag: &str| extract_xml_tag(&body_str, tag)
        .map(|v| v.trim().eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    db.set_public_access_block(
        bucket,
        flag("BlockPublicAcls"),
        flag("IgnorePublicAcls"),
        flag("BlockPublicPolicy"),
        flag("RestrictPublicBuckets"),
    )?;
    Ok(HttpResponse::Ok().insert_header(("Content-Length", "0")).body(""))
}

pub(super) async fn s3_delete_public_access_block_inner(bucket: &str, req: &HttpRequest) -> Result<HttpResponse, Error> {
    use crate::s3::auth::authenticate_s3_request;
    use crate::service::metadata_service::MetadataService;

    let auth_result = authenticate_s3_request(req).await?;
    let db = MetadataService::new(&auth_result.user_id)?;
    if let Err(resp) = require_bucket(&db, bucket) { return Ok(resp); }

    db.delete_public_access_block(bucket)?;
    Ok(HttpResponse::NoContent().insert_header(("Content-Length", "0")).body(""))
}

// ---------------------------------------------------------------------------
// Bucket policy status (ACL-derived only — no bucket policy engine yet)
// GET /s3/{bucket}?policyStatus
// ---------------------------------------------------------------------------

pub(super) async fn s3_get_bucket_policy_status_inner(bucket: &str, req: &HttpRequest) -> Result<HttpResponse, Error> {
    use crate::s3::auth::authenticate_s3_request;
    use crate::service::metadata_service::MetadataService;

    let auth_result = authenticate_s3_request(req).await?;
    let db = MetadataService::new(&auth_result.user_id)?;
    if let Err(resp) = require_bucket(&db, bucket) { return Ok(resp); }

    let grants = match db.get_bucket_acl(bucket) {
        Ok(Some((_, grants_json))) => grants_from_json(&grants_json),
        _ => Vec::new(),
    };
    let ignore_public_acls = matches!(db.get_public_access_block(bucket), Ok(Some((_, true, _, _))));
    let is_public = !ignore_public_acls && (is_publicly_readable(&grants) || is_authenticated_readable(&grants)
        || is_publicly_writable(&grants));

    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <PolicyStatus xmlns=\"{s3}\"><IsPublic>{p}</IsPublic></PolicyStatus>",
        s3 = S3_XMLNS, p = is_public,
    );
    Ok(HttpResponse::Ok().content_type("application/xml").body(xml))
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
    if bucket.contains("..") || bucket.contains("--") || bucket.contains(".-") || bucket.contains("-.") {
        return Err(s3_error(StatusCode::BAD_REQUEST, "InvalidBucketName",
                            "Bucket name cannot contain consecutive . or -, or mix . and - adjacently", bucket));
    }
    if !bucket.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-') {
        return Err(s3_error(StatusCode::BAD_REQUEST, "InvalidBucketName",
                            "Bucket name must only contain lowercase letters, numbers, hyphens, or dots", bucket));
    }
    // Reject names that look like an IPv4 address (e.g. "192.168.5.123") — real S3
    // rejects these since they'd be ambiguous with virtual-hosted-style IP endpoints.
    let octets: Vec<&str> = bucket.split('.').collect();
    if octets.len() == 4 && octets.iter().all(|o| !o.is_empty() && o.chars().all(|c| c.is_ascii_digit()) && o.parse::<u32>().map(|n| n <= 255).unwrap_or(false)) {
        return Err(s3_error(StatusCode::BAD_REQUEST, "InvalidBucketName",
                            "Bucket name cannot be formatted as an IP address", bucket));
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

