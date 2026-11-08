//! STS-compatible `AssumeRoleWithWebIdentity`: lets an operator point WarpDrive at
//! their own external OIDC identity provider (Keycloak, Dex, Okta, ...) instead of
//! WarpDrive owning user/credential management itself. Same shape MinIO and Ceph RGW
//! use, so an unmodified AWS SDK's STS client already knows this request/response
//! contract — a client calls `AssumeRoleWithWebIdentity` with an OIDC id_token, gets
//! back temporary SigV4 credentials, and speaks normal S3 from there.
//!
//! Deliberately scoped to the minimum that makes external-IAM configuration actually
//! work end to end: one configured issuer, claim-based bucket scoping, in-memory
//! sessions. Not covered here (separate, later work): bucket-policy evaluation,
//! multiple simultaneous IdPs, STS sessions over presigned URLs.

use actix_web::{web, Error, HttpRequest, HttpResponse};
use actix_web::http::StatusCode;
use jsonwebtoken::jwk::JwkSet;
use lazy_static::lazy_static;
use log::{debug, warn};
use parking_lot::RwLock;
use rand::Rng;
use std::collections::HashMap;
use std::time::{Duration, Instant};

lazy_static! {
    static ref HTTP_CLIENT: reqwest::Client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("failed to create reqwest client");
    static ref JWKS_CACHE: RwLock<Option<(JwkSet, Instant)>> = RwLock::new(None);
    /// One entry per temporary access_key_id, minted by AssumeRoleWithWebIdentity.
    static ref STS_SESSIONS: RwLock<HashMap<String, StsSession>> = RwLock::new(HashMap::new());
}

const JWKS_CACHE_TTL_SECS: u64 = 300;
const DEFAULT_SESSION_TTL_SECS: u64 = 3600;
const MIN_SESSION_TTL_SECS: u64 = 900;

#[derive(Clone)]
struct StsSession {
    secret_access_key: String,
    session_token: String,
    user_id: String,
    allowed_buckets: Vec<String>,
    expires_at: Instant,
}

/// What `auth.rs` needs to finish verifying a request carrying temporary credentials.
pub(crate) struct StsCredential {
    pub secret_access_key: String,
    pub session_token: String,
    pub user_id: String,
    pub allowed_buckets: Vec<String>,
}

/// Looks up a live (unexpired) STS session by its temporary access_key_id. Prunes the
/// entry if it has expired. Not a real auth decision by itself — the caller still has
/// to verify the SigV4 signature and the `X-Amz-Security-Token` header against it.
pub(crate) fn lookup_session(access_key: &str) -> Option<StsCredential> {
    let mut sessions = STS_SESSIONS.write();
    match sessions.get(access_key) {
        Some(s) if s.expires_at > Instant::now() => Some(StsCredential {
            secret_access_key: s.secret_access_key.clone(),
            session_token: s.session_token.clone(),
            user_id: s.user_id.clone(),
            allowed_buckets: s.allowed_buckets.clone(),
        }),
        Some(_) => {
            sessions.remove(access_key);
            None
        }
        None => None,
    }
}

struct OidcConfig {
    issuer: String,
    audience: String,
    jwks_url: String,
    /// Claim whose string value becomes `S3AuthResult::user_id`. Default "sub".
    user_claim: String,
    /// Optional claim (a JSON array of strings) that scopes the session to specific
    /// buckets. Falls back to `default_buckets` when absent or empty.
    buckets_claim: Option<String>,
    default_buckets: Vec<String>,
    session_ttl_secs: u64,
}

/// Reads `WARPDRIVE_OIDC_*`. Returns `None` if OIDC federation isn't configured on
/// this server (issuer/audience/jwks_url are all required together).
fn oidc_config_from_env() -> Option<OidcConfig> {
    let issuer = non_empty_env("WARPDRIVE_OIDC_ISSUER")?;
    let audience = non_empty_env("WARPDRIVE_OIDC_AUDIENCE")?;
    let jwks_url = non_empty_env("WARPDRIVE_OIDC_JWKS_URL")?;
    let user_claim = non_empty_env("WARPDRIVE_OIDC_USER_CLAIM").unwrap_or_else(|| "sub".to_string());
    let buckets_claim = non_empty_env("WARPDRIVE_OIDC_BUCKETS_CLAIM");
    let default_buckets = std::env::var("WARPDRIVE_OIDC_DEFAULT_BUCKETS")
        .ok()
        .map(|s| {
            s.split(',')
                .map(|b| b.trim().to_string())
                .filter(|b| !b.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let session_ttl_secs = std::env::var("WARPDRIVE_OIDC_SESSION_TTL_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_SESSION_TTL_SECS);
    Some(OidcConfig {
        issuer,
        audience,
        jwks_url,
        user_claim,
        buckets_claim,
        default_buckets,
        session_ttl_secs,
    })
}

fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.trim().is_empty())
}

async fn fetch_jwks(jwks_url: &str) -> Result<JwkSet, String> {
    {
        let cache = JWKS_CACHE.read();
        if let Some((ref set, fetched_at)) = *cache {
            if fetched_at.elapsed() < Duration::from_secs(JWKS_CACHE_TTL_SECS) {
                return Ok(set.clone());
            }
        }
    }
    let resp = HTTP_CLIENT
        .get(jwks_url)
        .send()
        .await
        .map_err(|e| format!("fetching JWKS from {jwks_url}: {e}"))?;
    let text = resp
        .text()
        .await
        .map_err(|e| format!("reading JWKS response body: {e}"))?;
    let set: JwkSet = serde_json::from_str(&text).map_err(|e| format!("parsing JWKS JSON: {e}"))?;
    *JWKS_CACHE.write() = Some((set.clone(), Instant::now()));
    Ok(set)
}

fn random_token(len: usize) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::thread_rng();
    (0..len)
        .map(|_| CHARS[rng.gen_range(0..CHARS.len())] as char)
        .collect()
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// `application/x-www-form-urlencoded` body parsing (what the real STS query
/// protocol sends — not JSON, not a query string). `+` means space per the spec.
fn parse_form_urlencoded(body: &[u8]) -> HashMap<String, String> {
    let body_str = String::from_utf8_lossy(body);
    body_str
        .split('&')
        .filter(|pair| !pair.is_empty())
        .filter_map(|pair| {
            let mut it = pair.splitn(2, '=');
            let k = it.next()?;
            let v = it.next().unwrap_or("");
            Some((form_decode(k), form_decode(v)))
        })
        .collect()
}

fn form_decode(s: &str) -> String {
    let with_spaces = s.replace('+', " ");
    percent_encoding::percent_decode_str(&with_spaces)
        .decode_utf8_lossy()
        .into_owned()
}

fn sts_error_response(status: StatusCode, code: &str, message: &str) -> HttpResponse {
    warn!("STS error: {code}: {message}");
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<ErrorResponse xmlns="https://sts.amazonaws.com/doc/2011-06-15/">
  <Error>
    <Type>Sender</Type>
    <Code>{code}</Code>
    <Message>{message}</Message>
  </Error>
  <RequestId>warpdrive-sts</RequestId>
</ErrorResponse>"#,
        code = xml_escape(code),
        message = xml_escape(message),
    );
    HttpResponse::build(status).content_type("text/xml").body(xml)
}

/// `POST /sts` — `Action=AssumeRoleWithWebIdentity`. Unauthenticated by design: this
/// endpoint *is* the authentication mechanism (the OIDC id_token is the credential).
pub async fn sts_assume_role_with_web_identity(
    _req: HttpRequest,
    body: web::Bytes,
) -> Result<HttpResponse, Error> {
    let form = parse_form_urlencoded(&body);

    let action = form.get("Action").map(String::as_str).unwrap_or("");
    if action != "AssumeRoleWithWebIdentity" {
        return Ok(sts_error_response(
            StatusCode::BAD_REQUEST,
            "InvalidAction",
            &format!("Unsupported Action '{action}'"),
        ));
    }

    let web_identity_token = match form.get("WebIdentityToken") {
        Some(t) if !t.is_empty() => t.clone(),
        _ => {
            return Ok(sts_error_response(
                StatusCode::BAD_REQUEST,
                "InvalidParameterValue",
                "WebIdentityToken is required",
            ))
        }
    };
    let role_session_name = form
        .get("RoleSessionName")
        .cloned()
        .unwrap_or_else(|| "warpdrive-session".to_string());
    let role_arn = form.get("RoleArn").cloned().unwrap_or_default();
    let requested_duration_secs = form.get("DurationSeconds").and_then(|s| s.parse::<u64>().ok());

    let config = match oidc_config_from_env() {
        Some(c) => c,
        None => {
            return Ok(sts_error_response(
                StatusCode::NOT_IMPLEMENTED,
                "InvalidIdentityToken",
                "OIDC federation is not configured on this server \
                 (set WARPDRIVE_OIDC_ISSUER, WARPDRIVE_OIDC_AUDIENCE, WARPDRIVE_OIDC_JWKS_URL)",
            ))
        }
    };

    let header = match jsonwebtoken::decode_header(&web_identity_token) {
        Ok(h) => h,
        Err(e) => {
            return Ok(sts_error_response(
                StatusCode::BAD_REQUEST,
                "InvalidIdentityToken",
                &format!("Malformed WebIdentityToken: {e}"),
            ))
        }
    };
    let kid = match header.kid {
        Some(ref k) => k.clone(),
        None => {
            return Ok(sts_error_response(
                StatusCode::BAD_REQUEST,
                "InvalidIdentityToken",
                "Token header is missing 'kid'",
            ))
        }
    };

    let jwks = match fetch_jwks(&config.jwks_url).await {
        Ok(j) => j,
        Err(e) => {
            return Ok(sts_error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "IDPCommunicationError",
                &format!("Could not fetch signing keys from the configured IdP: {e}"),
            ))
        }
    };
    let jwk = match jwks.find(&kid) {
        Some(j) => j,
        None => {
            return Ok(sts_error_response(
                StatusCode::BAD_REQUEST,
                "InvalidIdentityToken",
                "No signing key in the IdP's JWKS matches this token's 'kid'",
            ))
        }
    };
    let decoding_key = match jsonwebtoken::DecodingKey::from_jwk(jwk) {
        Ok(k) => k,
        Err(e) => {
            return Ok(sts_error_response(
                StatusCode::BAD_REQUEST,
                "InvalidIdentityToken",
                &format!("Unsupported signing key type: {e}"),
            ))
        }
    };

    let mut validation = jsonwebtoken::Validation::new(header.alg);
    validation.set_audience(&[config.audience.clone()]);
    validation.set_issuer(&[config.issuer.clone()]);
    let claims = match jsonwebtoken::decode::<serde_json::Value>(&web_identity_token, &decoding_key, &validation) {
        Ok(data) => data.claims,
        Err(e) => {
            return Ok(sts_error_response(
                StatusCode::FORBIDDEN,
                "InvalidIdentityToken",
                &format!("Token verification failed: {e}"),
            ))
        }
    };

    let user_id = match claims.get(&config.user_claim).and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => {
            return Ok(sts_error_response(
                StatusCode::FORBIDDEN,
                "InvalidIdentityToken",
                &format!("Token is missing the configured user claim '{}'", config.user_claim),
            ))
        }
    };

    let allowed_buckets: Vec<String> = config
        .buckets_claim
        .as_ref()
        .and_then(|claim_name| claims.get(claim_name))
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
        .filter(|v: &Vec<String>| !v.is_empty())
        .unwrap_or_else(|| config.default_buckets.clone());

    if allowed_buckets.is_empty() {
        return Ok(sts_error_response(
            StatusCode::FORBIDDEN,
            "AccessDenied",
            "No bucket scope resolved for this identity \
             (set WARPDRIVE_OIDC_DEFAULT_BUCKETS, or have the IdP issue the configured buckets claim)",
        ));
    }

    let ttl_secs = requested_duration_secs
        .unwrap_or(config.session_ttl_secs)
        .clamp(MIN_SESSION_TTL_SECS, config.session_ttl_secs.max(MIN_SESSION_TTL_SECS));

    let access_key_id = format!("ASIA{}", random_token(16).to_uppercase());
    let secret_access_key = random_token(40);
    let session_token = random_token(64);
    let expires_at = Instant::now() + Duration::from_secs(ttl_secs);
    let expiration_rfc3339 = (chrono::Utc::now() + chrono::Duration::seconds(ttl_secs as i64)).to_rfc3339();

    STS_SESSIONS.write().insert(
        access_key_id.clone(),
        StsSession {
            secret_access_key: secret_access_key.clone(),
            session_token: session_token.clone(),
            user_id: user_id.clone(),
            allowed_buckets: allowed_buckets.clone(),
            expires_at,
        },
    );
    debug!(
        "STS: issued session access_key={}... user_id={} buckets={:?} ttl_secs={}",
        &access_key_id[..8.min(access_key_id.len())],
        user_id,
        allowed_buckets,
        ttl_secs
    );

    let body_xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<AssumeRoleWithWebIdentityResponse xmlns="https://sts.amazonaws.com/doc/2011-06-15/">
  <AssumeRoleWithWebIdentityResult>
    <Credentials>
      <AccessKeyId>{access_key_id}</AccessKeyId>
      <SecretAccessKey>{secret_access_key}</SecretAccessKey>
      <SessionToken>{session_token}</SessionToken>
      <Expiration>{expiration}</Expiration>
    </Credentials>
    <SubjectFromWebIdentityToken>{user_id}</SubjectFromWebIdentityToken>
    <AssumedRoleUser>
      <AssumedRoleId>{role_session_name}:{user_id}</AssumedRoleId>
      <Arn>{role_arn}</Arn>
    </AssumedRoleUser>
    <Audience>{audience}</Audience>
  </AssumeRoleWithWebIdentityResult>
  <ResponseMetadata>
    <RequestId>warpdrive-sts</RequestId>
  </ResponseMetadata>
</AssumeRoleWithWebIdentityResponse>"#,
        access_key_id = xml_escape(&access_key_id),
        secret_access_key = xml_escape(&secret_access_key),
        session_token = xml_escape(&session_token),
        expiration = expiration_rfc3339,
        user_id = xml_escape(&user_id),
        role_session_name = xml_escape(&role_session_name),
        role_arn = xml_escape(&role_arn),
        audience = xml_escape(&config.audience),
    );

    Ok(HttpResponse::Ok().content_type("text/xml").body(body_xml))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_decode_handles_plus_as_space_and_percent_escapes() {
        assert_eq!(form_decode("a+b%20c"), "a b c");
        assert_eq!(form_decode("hello%3Dworld"), "hello=world");
    }

    #[test]
    fn parse_form_urlencoded_splits_pairs() {
        let body = b"Action=AssumeRoleWithWebIdentity&RoleSessionName=my+session&WebIdentityToken=abc.def.ghi";
        let form = parse_form_urlencoded(body);
        assert_eq!(form.get("Action").unwrap(), "AssumeRoleWithWebIdentity");
        assert_eq!(form.get("RoleSessionName").unwrap(), "my session");
        assert_eq!(form.get("WebIdentityToken").unwrap(), "abc.def.ghi");
    }

    #[test]
    fn lookup_session_prunes_expired_entries() {
        let access_key = "ASIA_TEST_EXPIRED".to_string();
        STS_SESSIONS.write().insert(
            access_key.clone(),
            StsSession {
                secret_access_key: "s".to_string(),
                session_token: "t".to_string(),
                user_id: "u".to_string(),
                allowed_buckets: vec!["b".to_string()],
                expires_at: Instant::now() - Duration::from_secs(1),
            },
        );
        assert!(lookup_session(&access_key).is_none());
        assert!(!STS_SESSIONS.read().contains_key(&access_key));
    }

    #[test]
    fn lookup_session_returns_live_entries() {
        let access_key = "ASIA_TEST_LIVE".to_string();
        STS_SESSIONS.write().insert(
            access_key.clone(),
            StsSession {
                secret_access_key: "s".to_string(),
                session_token: "t".to_string(),
                user_id: "u".to_string(),
                allowed_buckets: vec!["b".to_string()],
                expires_at: Instant::now() + Duration::from_secs(60),
            },
        );
        let cred = lookup_session(&access_key).expect("session should be live");
        assert_eq!(cred.user_id, "u");
        assert_eq!(cred.allowed_buckets, vec!["b".to_string()]);
    }
}
