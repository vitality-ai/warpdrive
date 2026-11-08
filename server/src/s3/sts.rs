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
    /// Serializes forced JWKS refreshes so concurrent kid-misses coalesce
    /// into one real outbound fetch instead of each firing their own, and
    /// tracks when the last *forced* refresh happened and whether it
    /// succeeded -- distinct from the general cache's own fetched_at
    /// (which a routine non-forced fetch also updates and so can't be used
    /// to rate-limit forced ones), and distinct from whether `JWKS_CACHE`
    /// merely has *some* value (which could be stale data from an earlier
    /// success, left over while the most recent forced attempt failed).
    static ref JWKS_REFRESH_LOCK: tokio::sync::Mutex<Option<(Instant, bool)>> = tokio::sync::Mutex::new(None);
    /// One entry per temporary access_key_id, minted by AssumeRoleWithWebIdentity.
    static ref STS_SESSIONS: RwLock<HashMap<String, StsSession>> = RwLock::new(HashMap::new());
}

const JWKS_CACHE_TTL_SECS: u64 = 300;
/// Floor on how often an unknown `kid` can force a fresh JWKS fetch at all,
/// regardless of request volume -- bounds the worst case to one outbound
/// request to the IdP per this many seconds.
const JWKS_FORCED_REFRESH_COOLDOWN_SECS: u64 = 5;
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

async fn fetch_jwks(jwks_url: &str, force_refresh: bool) -> Result<JwkSet, String> {
    if !force_refresh {
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

/// Coalesces concurrent forced refreshes into a single real fetch, and
/// rate-limits how often an unknown `kid` can trigger one at all. `/sts` is
/// unauthenticated by design (the token itself is the credential), so
/// without this an attacker could submit a stream of tokens with bogus `kid`
/// values and force an outbound request to the configured IdP's JWKS
/// endpoint on every single one.
async fn force_refresh_jwks_rate_limited(jwks_url: &str) -> Result<JwkSet, String> {
    let mut last_forced = JWKS_REFRESH_LOCK.lock().await;
    // Someone else may have already attempted a forced refresh while we
    // were waiting for the lock (or we did, moments ago, for a different
    // kid) -- if that happened within the cooldown window, don't make
    // another outbound request regardless of whether that attempt
    // succeeded. Recording the timestamp on failure too (below) is what
    // makes this reachable during an IdP outage: without it, a flood of
    // bogus-kid requests would retry the real (failing) fetch on every
    // single one for as long as the IdP stayed down -- exactly when it
    // least needs more load.
    if let Some((last, succeeded)) = *last_forced {
        if last.elapsed() < Duration::from_secs(JWKS_FORCED_REFRESH_COOLDOWN_SECS) {
            if succeeded {
                if let Some((ref set, _)) = *JWKS_CACHE.read() {
                    return Ok(set.clone());
                }
            }
            return Err(format!(
                "a forced JWKS refresh failed recently and is rate-limited; \
                 retry after the {JWKS_FORCED_REFRESH_COOLDOWN_SECS}s cooldown"
            ));
        }
    }
    let result = fetch_jwks(jwks_url, true).await;
    *last_forced = Some((Instant::now(), result.is_ok()));
    result
}

/// Resolves a signing key by `kid`. A cache miss doesn't immediately fail the
/// token: the IdP may have rotated its signing key since our cache was last
/// populated, so this forces one fresh (rate-limited, coalesced) fetch and
/// retries before giving up -- otherwise a real key rotation would deny
/// valid tokens for up to `JWKS_CACHE_TTL_SECS`.
async fn find_jwk(jwks_url: &str, kid: &str) -> Result<Option<jsonwebtoken::jwk::Jwk>, String> {
    let jwks = fetch_jwks(jwks_url, false).await?;
    if let Some(j) = jwks.find(kid) {
        return Ok(Some(j.clone()));
    }
    let jwks = force_refresh_jwks_rate_limited(jwks_url).await?;
    Ok(jwks.find(kid).cloned())
}

/// Shared between the handler and its tests so both exercise the exact same
/// validation rules.
///
/// Two gotchas this crate has, both handled here: `validate_nbf` defaults to
/// false (without it a correctly-signed token with a future "not before"
/// time would be accepted immediately), and `set_audience`/`set_issuer`
/// alone only check `aud`/`iss` *when present* -- a token that omits either
/// claim entirely still passes. `set_required_spec_claims` closes that:
/// `exp`/`iss`/`aud` must actually be in the token, not just valid if present.
fn build_validation(alg: jsonwebtoken::Algorithm, audience: &str, issuer: &str) -> jsonwebtoken::Validation {
    let mut validation = jsonwebtoken::Validation::new(alg);
    validation.set_audience(&[audience]);
    validation.set_issuer(&[issuer]);
    validation.validate_nbf = true;
    validation.set_required_spec_claims(&["exp", "iss", "aud"]);
    validation
}

/// Resolves the bucket scope for a session from the token's claims. Falls
/// back to the configured default scope only when the buckets claim is
/// *absent from the token entirely* -- if the IdP bothered to include it but
/// set it to an empty array or the wrong JSON type, that's an explicit
/// statement of "no access", not a reason to grant the (usually broader)
/// default scope instead. Either way, an empty result here is the caller's
/// cue to reject the request outright, not silently widen it.
fn resolve_allowed_buckets(claims: &serde_json::Value, buckets_claim: Option<&str>, default_buckets: &[String]) -> Vec<String> {
    match buckets_claim.and_then(|c| claims.get(c)) {
        Some(v) => v
            .as_array()
            .map(|arr| arr.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
            .unwrap_or_default(),
        None => default_buckets.to_vec(),
    }
}

/// Drops expired entries. Called on every new-session insertion rather than
/// only from `lookup_session`'s access path -- a normal refresh cycle mints a
/// new access_key_id each time, so an old, now-expired entry may never be
/// looked up again and would otherwise sit in the map for the life of the
/// process.
fn evict_expired_sessions(sessions: &mut HashMap<String, StsSession>) {
    let now = Instant::now();
    sessions.retain(|_, s| s.expires_at > now);
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

    let jwk = match find_jwk(&config.jwks_url, &kid).await {
        Ok(Some(j)) => j,
        Ok(None) => {
            return Ok(sts_error_response(
                StatusCode::BAD_REQUEST,
                "InvalidIdentityToken",
                "No signing key in the IdP's JWKS matches this token's 'kid', even after a forced refresh",
            ))
        }
        Err(e) => {
            return Ok(sts_error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "IDPCommunicationError",
                &format!("Could not fetch signing keys from the configured IdP: {e}"),
            ))
        }
    };
    let decoding_key = match jsonwebtoken::DecodingKey::from_jwk(&jwk) {
        Ok(k) => k,
        Err(e) => {
            return Ok(sts_error_response(
                StatusCode::BAD_REQUEST,
                "InvalidIdentityToken",
                &format!("Unsupported signing key type: {e}"),
            ))
        }
    };

    let validation = build_validation(header.alg, &config.audience, &config.issuer);
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

    let allowed_buckets = resolve_allowed_buckets(&claims, config.buckets_claim.as_deref(), &config.default_buckets);

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

    {
        let mut sessions = STS_SESSIONS.write();
        evict_expired_sessions(&mut sessions);
        sessions.insert(
            access_key_id.clone(),
            StsSession {
                secret_access_key: secret_access_key.clone(),
                session_token: session_token.clone(),
                user_id: user_id.clone(),
                allowed_buckets: allowed_buckets.clone(),
                expires_at,
            },
        );
    }
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

    lazy_static! {
        /// `JWKS_CACHE`/`JWKS_REFRESH_LOCK` are process-global singletons (by
        /// design -- one configured issuer), but Rust runs tests in this
        /// module on multiple threads by default. Without this, two tests
        /// that each spin up their own fake JWKS server and populate the one
        /// shared cache race and intermittently fail depending on which
        /// one's state the other reads mid-test. Every test that touches
        /// `find_jwk`/`fetch_jwks` holds this for its full duration.
        static ref JWKS_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    }

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

    #[test]
    fn evict_expired_sessions_drops_only_expired_entries() {
        let mut sessions = HashMap::new();
        let make = |secs_from_now: i64| StsSession {
            secret_access_key: "s".to_string(),
            session_token: "t".to_string(),
            user_id: "u".to_string(),
            allowed_buckets: vec!["b".to_string()],
            expires_at: if secs_from_now >= 0 {
                Instant::now() + Duration::from_secs(secs_from_now as u64)
            } else {
                Instant::now() - Duration::from_secs((-secs_from_now) as u64)
            },
        };
        sessions.insert("expired-1".to_string(), make(-10));
        sessions.insert("expired-2".to_string(), make(-1));
        sessions.insert("live".to_string(), make(60));

        evict_expired_sessions(&mut sessions);

        assert_eq!(sessions.len(), 1);
        assert!(sessions.contains_key("live"));
    }

    // --- RS256 fixtures (test-only keypairs, no relation to any real IdP) ---

    const TEST_PRIV_PEM_A: &str = "-----BEGIN PRIVATE KEY-----
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQC77JCKKqLO5Ew9
OJO17km+bykWertF5Na8aDmAob5li2ZEntIQsPBWvt0XVk7LuHRfK+bmVXP8NFn5
0J5iyge0M0kDqBwKj0fBB2aqt0uJBX0Hbi8RV5+gOgY6DWyybvgc9wnKy3C5VxXn
Kvo/iYBrfVq90V3sz/0DnLyajPyFFMYAJq5sYPuWz+umysC0t1CxBYj97UV+/9Dy
iS9o9x0DEyJ9Hbh82eVfgSxVXYsaxrk2jpwRYWX0wqjQWXfqpFrxVfQsGu473JVP
UtXCM9doZ+8dNZ5o9PAV1HsNWtezkLFhMFt5m5H3x+G0Sf8m9VR+PJJNdJPn2MGx
iu9mfx5/AgMBAAECggEAP2Y6BUMJFj+TMPxLaQ9xNZwPApR5ZcUkZP/Mlob60wtK
f1ANkq//BWTy9hKaFsSuDgWjqoWd0bJfwJYPOUjrfHHo6iDnBPD9xjOXAXVBOILv
gXg8zN8A7W9l9mPdDf2+GxeE6DPmXcDxlMG9F35uItVxraRaybAJrhRM1N9HuiWJ
JyFltRbicFb9sz3R1Vq3D76QyteBMwdnlE7ocfG22nXMSE87Av+84wEXDTFKLbz7
ZW7WOoinZiNrBwGVfJlKV4b9v/gYGceg4m2D+BvMqgrYy6igXlgyv4DW37ocUl8s
1qSlKVPjaITF7KbKYcm90H+t/0GFFlCytePGGMPvUQKBgQD0G73mHOZt5YzzLyG8
+5K6OvUyn6DZQu532Z+/zHWrhxsfRNBrv+atgeBkolZ4qmZyqIUDN49ECSff1D1+
PRNJNnvnjNUppE1657naorc65CKPbYsPA6X+/uqLgYUHxg+XKMtbpQ8g9X4F0Qu0
71lXr77Jmr89n1ILBpuzho02PQKBgQDFFCcNqO1ZENBR8XK447jhRqSJHLEEHWam
sxXLxCAVEBih0hHXMNeGGr1PDGLlhpfirJQsERwBAVD+W0zmdKNOWGVqiRz278pC
7b259rXG5U3ixZRUQ8swc0qobLlaDabnWLpD9VOhAjGbxdNir/pJhdSpv+Pm1LKb
c5V0XxlvawKBgH7FhXpi2dyK0QdeG8RcVhf3FG3E7rDGDLgbQdZGpXl+AbzcbOz4
TSEgiNtaX1PtEqVLFUDVcsafhqC8RW8w4ror3G8k+XLVzBMfEqsTw9qGrdgUyJn0
iBmEY2Qt0Ft4JZKX8fxXHSM9usPki5LclmZuc8OZZOXovp/RP70dDGr1AoGAcnnG
RZT+aWHu3+pULTl5xsF2CX71A+LgIayc2IPP7uwuorCXVFw2lNsrY6NaPuDzkOOW
HhLCJYNSPK1j+el+gp+uKTgD7xxSbJYUQtUfuaywlS0Xf+rxN++ASlz9PWX4sSFE
8Pyr2zafAlDi/MwjollusjuYchjStqFpCvPCnokCgYEAsFB9CbgzwFIc7W+oSBSq
pOJg7ErDvEqRM3qGT37QuMSA/frXqBm4mKt8i4bizrw3Q5+6PLeUWywgZJ7rQWzK
90VAzzeJ+dN+vU89cbGnZpD4g80HXJKNYLllgFKJ2Ye7rCmdUxNrzh/+sk2iAWDi
Aw4iZ2K6Bxj1vR5UsdcApR0=
-----END PRIVATE KEY-----";
    const TEST_N_A: &str = "\
u-yQiiqizuRMPTiTte5Jvm8pFnq7ReTWvGg5gKG-ZYtmRJ7SELDwVr7dF1ZOy7h0\
Xyvm5lVz_DRZ-dCeYsoHtDNJA6gcCo9HwQdmqrdLiQV9B24vEVefoDoGOg1ssm74\
HPcJystwuVcV5yr6P4mAa31avdFd7M_9A5y8moz8hRTGACaubGD7ls_rpsrAtLdQ\
sQWI_e1Ffv_Q8okvaPcdAxMifR24fNnlX4EsVV2LGsa5No6cEWFl9MKo0Fl36qRa\
8VX0LBruO9yVT1LVwjPXaGfvHTWeaPTwFdR7DVrXs5CxYTBbeZuR98fhtEn_JvVU\
fjySTXST59jBsYrvZn8efw";
    const TEST_E: &str = "AQAB";
    const TEST_KID_A: &str = "test-kid-a";

    const TEST_PRIV_PEM_B: &str = "-----BEGIN PRIVATE KEY-----
MIIEvwIBADANBgkqhkiG9w0BAQEFAASCBKkwggSlAgEAAoIBAQDRSORVXcUqws/u
0sg/pT+i6/L0O1qwPuWtvGXd/DuOXxAdp9dqhuhfpycA7RuAxqqa+RIwwJCItGp/
aNeaeXK7slbsEHgQrCLqPYCHN/5c0wrOpq8AtP7UG8SQPl44CN3JRPiJRlkPu1j/
meEkJCUxc0QkZrsqPJS/rSBaziXjPsVeEMla5ZyYF8IhhobhRraGu3VxkCboHNOq
BXJdNRN1TUVKc6mMq9QR7cGhP1NC+hpKB22eclhfISHop6p8uTpcP9V1aaW4mIOT
s4wDio/uj9yqKi5Z3pcB86yJAerYDtOGj61nDd/BxqAVf23EDxslKplQJOrCxaIY
RoIMGofxAgMBAAECggEAAnwFCeZlEs7hPQqPigHiMYLvjWqu9uyoOVnFM89svZHv
VQrLCd60H36SffZSn0K8f755Ms72aEkMApsyBAlxDT1evsqrQjdo6rL0Naa6hEuV
E0LzTOTZKZdyy+uRnsSsLVl+QdLt97vXoKEpBQ7Qyjd+FZiKnEO/2E6r1agg1wPA
PBD57WewZafNHN/IU+djGJ0MUoG6MXbmuVaIeXeo9Oo5b1SwJABExpnVwnCmUkOs
dM1DCP1dY5SYlBEU6Sa+Vgx7C/m5/Mr1gRwXA4L1VLtGeTSmX+FE4HzZ5RgPKDhI
eAZiSOBnegPkr6+ciQ2WW2XikS6kZmJgh6N60vu1YQKBgQD8DfQh79yOwvpQ5poC
OzgXJzD/ova+UpzoTNsu/O7lVRzqHK8bd4blOBXXSlXUXpMz5AnCrLpOHnrWhjL3
ERf7+oLjB6vHPy1Ompi4VGVNw1rNAzvPEMPi7D/ceGsWzEoSflCw3S2yf/zNWQPr
a9vrH/bHReFGd2gpdPHWTdtv3QKBgQDUj4yIXK71stQkArDk1YizHa8BeF9D/xfg
wXC9yDWWkvelCDc86OrDydkc7CpllW6PX4Cuz13BjP5tYznO+931En9iJ7qXOpaL
KUeJc0SR5pDMXiu2o2GHagdD6HXXPqBVALny6RK77RnhWlPzUZQfPmCVJNkq3d1/
3zbnekmBJQKBgQCnktNTWLfnBgQLkzSi9SIWaozt+DdZU21UF/zE4Y4mVA7UKtvU
RAQgEsiaiKZFedcd8ab3kYSPeT4rr67kia3LbcJlE+2FmyARGUEdk6aCK7YLhPHX
eYryLCVM+5Ye4xT/9JDK9VFzUhoNn/e2JdBhIrAbzf62rZ5HqaM62ipg1QKBgQCX
WGpggVWV3bGMvlnDZYMCvcTo0BNMr+oJftJOXqpBPv2TBQmHlfPqWeL74+/ACqkA
7qgKT93avRA/RKJZjR4ZrAUFF3KU0QN1rHuiE2+kTr2Utgx761K6/zSTFoYpLw8r
1uuPWIKdShHeUdlqaxCDOn+8UdNK4VQgAwftwzVo/QKBgQDhxUIGE0RuiJI8bu38
f7QnWkZoyj0VMXAZECo8zVhKz+N3Vvmawy3OH9QgCxCBeN4AtF3f1qaZcsU6fu6J
T7HcxCF1WY/hFrPB95BmOUI1boySrAZTTKnZ2KOY7Y2qfhVyTm0WjQskzv30W5+i
xosKqnn9mbPFjt/fL3klOFa7Fw==
-----END PRIVATE KEY-----";
    const TEST_N_B: &str = "\
0UjkVV3FKsLP7tLIP6U_ouvy9DtasD7lrbxl3fw7jl8QHafXaoboX6cnAO0bgMaq\
mvkSMMCQiLRqf2jXmnlyu7JW7BB4EKwi6j2Ahzf-XNMKzqavALT-1BvEkD5eOAjd\
yUT4iUZZD7tY_5nhJCQlMXNEJGa7KjyUv60gWs4l4z7FXhDJWuWcmBfCIYaG4Ua2\
hrt1cZAm6BzTqgVyXTUTdU1FSnOpjKvUEe3BoT9TQvoaSgdtnnJYXyEh6KeqfLk6\
XD_VdWmluJiDk7OMA4qP7o_cqiouWd6XAfOsiQHq2A7Tho-tZw3fwcagFX9txA8b\
JSqZUCTqwsWiGEaCDBqH8Q";
    const TEST_KID_B: &str = "test-kid-b";

    const TEST_ISSUER: &str = "https://test-idp.example/";
    const TEST_AUDIENCE: &str = "test-audience";

    fn sign_test_token(priv_pem: &str, kid: &str, nbf_offset_secs: i64, exp_offset_secs: i64) -> String {
        #[derive(serde::Serialize)]
        struct Claims<'a> {
            sub: &'a str,
            aud: &'a str,
            iss: &'a str,
            nbf: i64,
            exp: i64,
            iat: i64,
        }
        let now = chrono::Utc::now().timestamp();
        let claims = Claims {
            sub: "test-subject",
            aud: TEST_AUDIENCE,
            iss: TEST_ISSUER,
            nbf: now + nbf_offset_secs,
            exp: now + exp_offset_secs,
            iat: now,
        };
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
        header.kid = Some(kid.to_string());
        let key = jsonwebtoken::EncodingKey::from_rsa_pem(priv_pem.as_bytes()).expect("valid test private key");
        jsonwebtoken::encode(&header, &claims, &key).expect("signing a test token should never fail")
    }

    #[test]
    fn build_validation_rejects_a_not_yet_valid_token() {
        let decoding_key = jsonwebtoken::DecodingKey::from_rsa_components(TEST_N_A, TEST_E).unwrap();

        // Signed validly, but with nbf 5 minutes in the future.
        let future_token = sign_test_token(TEST_PRIV_PEM_A, TEST_KID_A, 300, 3600);
        let validation = build_validation(jsonwebtoken::Algorithm::RS256, TEST_AUDIENCE, TEST_ISSUER);
        let result = jsonwebtoken::decode::<serde_json::Value>(&future_token, &decoding_key, &validation);
        assert!(result.is_err(), "a token not yet valid (future nbf) must be rejected");

        // Sanity: the same validation accepts an otherwise-identical token
        // whose nbf has already passed, so this isn't failing for some
        // unrelated reason (wrong audience/issuer/signature).
        let valid_token = sign_test_token(TEST_PRIV_PEM_A, TEST_KID_A, -60, 3600);
        let result = jsonwebtoken::decode::<serde_json::Value>(&valid_token, &decoding_key, &validation);
        assert!(result.is_ok(), "a token with a past nbf should be accepted: {:?}", result.err());
    }

    #[test]
    fn build_validation_requires_exp_iss_and_aud_to_actually_be_present() {
        // set_audience()/set_issuer() alone only validate those claims *if
        // present* -- a token omitting them entirely still passed before
        // set_required_spec_claims was added. Sign one with sub+exp only,
        // no iss/aud at all (unlike sign_test_token, which always includes
        // them), and confirm it's now rejected.
        #[derive(serde::Serialize)]
        struct ClaimsMissingIssAud {
            sub: &'static str,
            exp: i64,
        }
        let claims = ClaimsMissingIssAud {
            sub: "test-subject",
            exp: chrono::Utc::now().timestamp() + 3600,
        };
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
        header.kid = Some(TEST_KID_A.to_string());
        let key = jsonwebtoken::EncodingKey::from_rsa_pem(TEST_PRIV_PEM_A.as_bytes()).unwrap();
        let token_missing_iss_aud = jsonwebtoken::encode(&header, &claims, &key).unwrap();

        let decoding_key = jsonwebtoken::DecodingKey::from_rsa_components(TEST_N_A, TEST_E).unwrap();
        let validation = build_validation(jsonwebtoken::Algorithm::RS256, TEST_AUDIENCE, TEST_ISSUER);
        let result = jsonwebtoken::decode::<serde_json::Value>(&token_missing_iss_aud, &decoding_key, &validation);
        assert!(result.is_err(), "a token with no iss/aud claims at all must be rejected, not silently accepted");

        // Sanity: a token with all three present and correct still passes.
        let full_token = sign_test_token(TEST_PRIV_PEM_A, TEST_KID_A, -60, 3600);
        let result = jsonwebtoken::decode::<serde_json::Value>(&full_token, &decoding_key, &validation);
        assert!(result.is_ok(), "a token with all required claims present should be accepted: {:?}", result.err());
    }

    #[test]
    fn resolve_allowed_buckets_falls_back_to_default_only_when_claim_is_truly_absent() {
        let default_buckets = vec!["default-bucket".to_string()];

        // No buckets_claim configured at all -> default.
        let claims = serde_json::json!({"sub": "x"});
        assert_eq!(resolve_allowed_buckets(&claims, None, &default_buckets), default_buckets);

        // buckets_claim configured, but the key isn't in the token -> default.
        assert_eq!(
            resolve_allowed_buckets(&claims, Some("buckets"), &default_buckets),
            default_buckets
        );

        // Claim present with real values -> those, not the default.
        let claims = serde_json::json!({"buckets": ["a", "b"]});
        assert_eq!(
            resolve_allowed_buckets(&claims, Some("buckets"), &default_buckets),
            vec!["a".to_string(), "b".to_string()]
        );

        // Claim present but an empty array -> explicit deny (empty), not a
        // fallback to the broader default scope.
        let claims = serde_json::json!({"buckets": []});
        assert_eq!(
            resolve_allowed_buckets(&claims, Some("buckets"), &default_buckets),
            Vec::<String>::new()
        );

        // Claim present but the wrong JSON type -> also an explicit deny.
        let claims = serde_json::json!({"buckets": "not-an-array"});
        assert_eq!(
            resolve_allowed_buckets(&claims, Some("buckets"), &default_buckets),
            Vec::<String>::new()
        );
    }

    fn jwks_json(entries: &[(&str, &str)]) -> String {
        let keys: Vec<String> = entries
            .iter()
            .map(|(kid, n)| {
                format!(
                    r#"{{"kty":"RSA","use":"sig","alg":"RS256","kid":"{kid}","n":"{n}","e":"{TEST_E}"}}"#
                )
            })
            .collect();
        format!(r#"{{"keys":[{}]}}"#, keys.join(","))
    }

    /// Serves `bodies[0]` for the first request, `bodies[1]` for every
    /// request after that -- stands in for an IdP whose JWKS endpoint
    /// starts returning a newly-rotated key set partway through the test.
    /// Also counts real requests received, so a test can assert that a
    /// burst of calls produced far fewer outbound fetches than calls.
    fn spawn_rotating_jwks_server(bodies: Vec<String>) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local_addr");
        let request_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let request_count_for_server = request_count.clone();
        std::thread::spawn(move || {
            let mut call = 0usize;
            for stream in listener.incoming() {
                let mut stream = match stream {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let mut buf = [0u8; 1024];
                let _ = std::io::Read::read(&mut stream, &mut buf);
                request_count_for_server.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let body = bodies[call.min(bodies.len() - 1)].clone();
                call += 1;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = std::io::Write::write_all(&mut stream, response.as_bytes());
            }
        });
        (format!("http://{addr}/jwks"), request_count)
    }

    #[tokio::test]
    async fn find_jwk_force_refreshes_on_a_cache_miss_but_rate_limits_and_coalesces_it() {
        let _guard = JWKS_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        *JWKS_CACHE.write() = None;
        *JWKS_REFRESH_LOCK.lock().await = None;
        let (jwks_url, request_count) = spawn_rotating_jwks_server(vec![
            jwks_json(&[(TEST_KID_A, TEST_N_A)]),             // before "rotation": only key A
            jwks_json(&[(TEST_KID_A, TEST_N_A), (TEST_KID_B, TEST_N_B)]), // after: A and B
        ]);

        // Primes the cache with the pre-rotation JWKS (key A only).
        let found_a = find_jwk(&jwks_url, TEST_KID_A).await.expect("fetch ok");
        assert!(found_a.is_some());
        assert_eq!(request_count.load(std::sync::atomic::Ordering::SeqCst), 1);

        // Key B isn't in the cached set yet -- this must force a fresh
        // fetch and find it there, not just report it missing because the
        // (still within-TTL) cached set doesn't have it.
        let found_b = find_jwk(&jwks_url, TEST_KID_B).await.expect("fetch ok");
        assert!(
            found_b.is_some(),
            "a newly-rotated-in key should be found via a forced refresh, not rejected"
        );
        assert_eq!(request_count.load(std::sync::atomic::Ordering::SeqCst), 2);

        // A flood of lookups for a kid that will never exist, fired
        // concurrently -- every one of them hits the cache-miss path. The
        // refresh we just did above is still within the cooldown window, so
        // none of these should make a real outbound request: an
        // unauthenticated caller spamming bogus key IDs must not be able to
        // force one IdP request per attempt.
        let mut handles = Vec::new();
        for _ in 0..20 {
            let url = jwks_url.clone();
            handles.push(tokio::spawn(async move { find_jwk(&url, "nonexistent-kid").await }));
        }
        for h in handles {
            let result = h.await.expect("task join").expect("fetch ok");
            assert!(result.is_none(), "a truly nonexistent kid must still resolve to None");
        }
        let count_after_flood = request_count.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            count_after_flood <= 3,
            "20 bogus-kid lookups within the cooldown window should produce only a handful of \
             real fetches via coalescing/rate-limiting, got {count_after_flood}"
        );
    }

    #[tokio::test]
    async fn force_refresh_rate_limit_also_applies_after_a_failed_attempt() {
        let _guard = JWKS_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        *JWKS_CACHE.write() = None;
        *JWKS_REFRESH_LOCK.lock().await = None;
        let (jwks_url, request_count) = spawn_rotating_jwks_server(vec![
            jwks_json(&[(TEST_KID_A, TEST_N_A)]), // first request succeeds, seeds the cache
            "not valid json".to_string(),         // every request after that fails to parse
        ]);

        // Seed the cache with a normal (non-forced) fetch.
        let found_a = find_jwk(&jwks_url, TEST_KID_A).await.expect("fetch ok");
        assert!(found_a.is_some());
        assert_eq!(request_count.load(std::sync::atomic::Ordering::SeqCst), 1);

        // Kid B isn't cached -- forces a refresh, which now hits the
        // broken server and fails. This is the attempt that must still get
        // recorded for rate-limiting purposes even though it failed.
        let result = find_jwk(&jwks_url, TEST_KID_B).await;
        assert!(result.is_err(), "a failing JWKS endpoint must surface as an error, not panic or silently succeed");
        assert_eq!(request_count.load(std::sync::atomic::Ordering::SeqCst), 2);

        // A flood of further kid-misses while the endpoint is still
        // broken -- every one of them must still be rate-limited, not
        // retry the real (failing) fetch on every single attempt.
        let mut handles = Vec::new();
        for _ in 0..20 {
            let url = jwks_url.clone();
            handles.push(tokio::spawn(async move { find_jwk(&url, "still-nonexistent").await }));
        }
        for h in handles {
            let result = h.await.expect("task join");
            assert!(result.is_err(), "a failing JWKS endpoint must keep surfacing as an error, not go quiet");
        }
        let count_after_flood = request_count.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            count_after_flood <= 4,
            "20 lookups against a failing endpoint, within the cooldown window, should still only \
             produce a handful of real attempts, got {count_after_flood}"
        );
    }
}
