// S3 Authentication module
use actix_web::{HttpRequest, HttpResponse, Error, error::{ErrorBadRequest, ErrorForbidden, ErrorServiceUnavailable, ErrorUnauthorized}};
use lazy_static::lazy_static;
use log::{debug, warn};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::sync::RwLock;
use std::time::{Duration, Instant};

lazy_static! {
    static ref HTTP_CLIENT: reqwest::Client = reqwest::Client::builder()
        // Protect Actix workers from hanging indefinitely when Console is slow/unreachable.
        .timeout(Duration::from_secs(5))
        .build()
        .expect("failed to create reqwest client");
    /// One entry per `access_key`: secret, owner, and Console-registered bucket names (refreshed on TTL).
    static ref CREDENTIAL_CACHE: RwLock<HashMap<String, CachedCredential>> = RwLock::new(HashMap::new());
}

/// Default TTL for credential cache (seconds)
const DEFAULT_CACHE_TTL_SECS: u64 = 300;

#[derive(Clone)]
struct CachedCredential {
    secret_key: String,
    owner_id: String,
    /// Names from Console `s3-credentials` response (`registered_buckets`).
    allowed_buckets: HashSet<String>,
    expires_at: Instant,
}

/// Response from Vitality Console `s3-credentials` endpoint.
#[derive(Debug, Deserialize)]
struct S3CredentialsResponse {
    owner_id: String,
    secret_key: String,
    /// All bucket names for this owner from Console `buckets` (includes `default` when present).
    #[serde(default, alias = "registeredBuckets")]
    registered_buckets: Vec<String>,
}

/// S3 Authentication result
#[derive(Debug)]
pub struct S3AuthResult {
    pub access_key: String,
    pub user_id: String,
    pub bucket: String,
    /// Snapshot of Console-registered buckets for this key (from credential cache at refresh time).
    pub allowed_buckets: Vec<String>,
    /// True for the hardcoded admin user — skips bucket membership checks so any bucket is reachable.
    pub allow_all_buckets: bool,
}

/// Returns (base_url, service_secret, cache_ttl_secs).
/// VITALITY_CONSOLE_URL and WARPDRIVE_SERVICE_SECRET are required (single auth path: Console + SigV4).
fn auth_config_from_env() -> (Option<String>, Option<String>, u64) {
    let base_url = std::env::var("VITALITY_CONSOLE_URL")
        .ok()
        .filter(|u| !u.trim().is_empty());
    let service_secret = std::env::var("WARPDRIVE_SERVICE_SECRET").ok();
    let cache_ttl_secs = std::env::var("S3_AUTH_CACHE_TTL_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_CACHE_TTL_SECS);
    if let Some(ref u) = base_url {
        debug!(
            "S3 auth: Vitality Console at {} (cache per access_key: secret + registered bucket set)",
            u
        );
    }
    (base_url, service_secret, cache_ttl_secs)
}

fn access_key_log_prefix(access_key: &str) -> String {
    if access_key.is_empty() {
        return "<empty>".to_string();
    }
    const MAX: usize = 10;
    let mut it = access_key.chars();
    let prefix: String = it.by_ref().take(MAX).collect();
    if it.next().is_some() {
        format!("{}…", prefix)
    } else {
        prefix
    }
}

/// One Console round-trip: `POST .../s3-credentials` with `access_key` only (no `bucket` field).
async fn fetch_credential_bundle_from_console(
    access_key: &str,
    base_url: &str,
    service_secret: &str,
) -> Result<(String, String, Vec<String>), Error> {
    let url = format!("{}/api/auth/s3-credentials", base_url.trim_end_matches('/'));
    let ak_p = access_key_log_prefix(access_key);
    let body_json = serde_json::json!({ "access_key": access_key });
    debug!(
        "Console s3-credentials (bundle) → POST {} access_key={}",
        url, ak_p
    );
    let res = HTTP_CLIENT
        .post(&url)
        .header("X-Warpdrive-Secret", service_secret)
        .json(&body_json)
        .send()
        .await
        .map_err(|e| {
            warn!("Vitality Console s3-credentials (bundle) request failed: {}", e);
            ErrorUnauthorized("Authentication service unavailable")
        })?;
    let status = res.status();
    if !status.is_success() {
        warn!("Console s3-credentials (bundle) ← HTTP {} access_key={}", status, ak_p);
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(ErrorUnauthorized("Invalid access key or service secret"));
        }
        if status == reqwest::StatusCode::REQUEST_TIMEOUT
            || status == reqwest::StatusCode::TOO_MANY_REQUESTS
            || status.is_server_error()
        {
            return Err(ErrorServiceUnavailable("Authentication service unavailable"));
        }
        return Err(ErrorUnauthorized("Authentication request rejected"));
    }
    let body: S3CredentialsResponse = res.json().await.map_err(|e| {
        warn!("Failed to parse s3-credentials (bundle) response: {}", e);
        ErrorServiceUnavailable("Invalid response from authentication service")
    })?;
    let buckets = body.registered_buckets;
    if buckets.is_empty() {
        warn!(
            "Console s3-credentials bundle returned registered_buckets=[] — owner has no rows in Console buckets table (create `default` or another bucket in the UI)"
        );
    }
    debug!(
        "Console s3-credentials (bundle) ← HTTP 200 owner_id={} registered_buckets={:?}",
        body.owner_id, buckets
    );
    Ok((body.owner_id, body.secret_key, buckets))
}

fn invalidate_s3_credential_cache(access_key: &str) {
    if let Ok(mut cache) = CREDENTIAL_CACHE.write() {
        if cache.remove(access_key).is_some() {
            debug!(
                "S3 credential cache INVALIDATED access_key={}",
                access_key_log_prefix(access_key)
            );
        }
    }
}

/// Load secret + owner + bucket allowlist from cache, or refresh from Console.
///
/// Refresh: **one** `POST .../s3-credentials` (response includes `registered_buckets` from Console `buckets` table).
/// Second value is `true` if the row was served from a non-expired cache entry.
async fn load_or_refresh_credential_bundle(
    access_key: &str,
    base_url: &str,
    service_secret: &str,
    cache_ttl_secs: u64,
) -> Result<(String, String, HashSet<String>, bool), Error> {
    let cache_key = access_key.to_string();
    {
        let cached = CREDENTIAL_CACHE.read().map_err(|_| ErrorUnauthorized("Cache lock"))?;
        if let Some(c) = cached.get(&cache_key) {
            if c.expires_at > Instant::now() {
                debug!(
                    "S3 credential cache HIT access_key={} owner={} allowed_bucket_count={}",
                    access_key_log_prefix(access_key),
                    c.owner_id,
                    c.allowed_buckets.len()
                );
                return Ok((
                    c.owner_id.clone(),
                    c.secret_key.clone(),
                    c.allowed_buckets.clone(),
                    true,
                ));
            }
        }
    }

    debug!(
        "S3 credential cache MISS/EXPIRED access_key={} — refreshing bundle from Console",
        access_key_log_prefix(access_key)
    );
    let (owner_id, secret_key, names) =
        fetch_credential_bundle_from_console(access_key, base_url, service_secret).await?;
    let allowed_buckets: HashSet<String> = names.into_iter().collect();

    let expires_at = Instant::now() + Duration::from_secs(cache_ttl_secs);
    let mut cache = CREDENTIAL_CACHE.write().map_err(|_| ErrorUnauthorized("Cache lock"))?;
    cache.insert(
        cache_key,
        CachedCredential {
            secret_key: secret_key.clone(),
            owner_id: owner_id.clone(),
            allowed_buckets: allowed_buckets.clone(),
            expires_at,
        },
    );
    debug!(
        "S3 credential cache REFRESHED access_key={} owner={} allowed_buckets={:?}",
        access_key_log_prefix(access_key),
        owner_id,
        allowed_buckets
    );
    Ok((owner_id, secret_key, allowed_buckets, false))
}

/// Parsed components from Authorization header for SigV4 verification
struct ParsedAuthHeader {
    access_key: String,
    date: String,       // YYYYMMDD
    region: String,
    service: String,
    signed_headers: Vec<String>,
    signature: String,
}

fn parse_authorization_header_full(auth_header: &str) -> Result<ParsedAuthHeader, Error> {
    if !auth_header.starts_with("AWS4-HMAC-SHA256") {
        return Err(ErrorUnauthorized("Invalid authorization format"));
    }
    // Credential=AccessKey/YYYYMMDD/region/service/aws4_request
    let credential_start = auth_header.find("Credential=").ok_or_else(|| ErrorUnauthorized("Missing Credential"))?;
    let credential_part = &auth_header[credential_start + 11..];
    let credential_end = credential_part.find(',').unwrap_or(credential_part.len());
    let credential = credential_part[..credential_end].trim();
    let parts: Vec<&str> = credential.splitn(2, '/').collect();
    let access_key = parts.get(0).ok_or_else(|| ErrorUnauthorized("Invalid Credential"))?.trim().to_string();
    if access_key.is_empty() {
        return Err(ErrorUnauthorized("Invalid Credential: access key is empty"));
    }
    let scope = parts.get(1).ok_or_else(|| ErrorUnauthorized("Invalid Credential"))?;
    let scope_parts: Vec<&str> = scope.splitn(4, '/').collect();
    let date = scope_parts.get(0).ok_or_else(|| ErrorUnauthorized("Invalid Credential"))?.to_string();
    let region = scope_parts.get(1).unwrap_or(&"us-east-1").to_string();
    let service = scope_parts.get(2).unwrap_or(&"s3").to_string();

    let signed_headers_start = auth_header.find("SignedHeaders=").ok_or_else(|| ErrorUnauthorized("Missing SignedHeaders"))?;
    let signed_part = &auth_header[signed_headers_start + 14..];
    let signed_end = signed_part.find(',').unwrap_or(signed_part.len());
    let signed_headers_str = signed_part[..signed_end].trim();
    let signed_headers: Vec<String> = signed_headers_str
        .split(';')
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    if signed_headers.is_empty() {
        return Err(ErrorUnauthorized("SignedHeaders must list at least one header"));
    }

    let sig_start = auth_header.find("Signature=").ok_or_else(|| ErrorUnauthorized("Missing Signature"))?;
    let sig_part = &auth_header[sig_start + 10..];
    let signature = sig_part.split(',').next().unwrap_or(sig_part).trim().to_string();

    Ok(ParsedAuthHeader {
        access_key,
        date,
        region,
        service,
        signed_headers,
        signature,
    })
}

/// Parse S3 Authorization header; returns `(access_key, signature)` from SigV4.
pub fn parse_authorization_header(auth_header: &str) -> Result<(String, String), Error> {
    let p = parse_authorization_header_full(auth_header)?;
    Ok((p.access_key, p.signature))
}

/// SHA256 hash of empty body (for GET/HEAD etc.)
const EMPTY_PAYLOAD_HASH: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// Verify AWS SigV4 signature. Returns Ok(()) if the request signature matches.
fn verify_sigv4(
    req: &HttpRequest,
    secret_key: &str,
    parsed: &ParsedAuthHeader,
) -> Result<(), Error> {
    use hmac::{Hmac, Mac};
    use sha2::{Digest, Sha256};

    type HmacSha256 = Hmac<Sha256>;

    let payload_hash = req
        .headers()
        .get("x-amz-content-sha256")
        .and_then(|v| v.to_str().ok())
        .unwrap_or(EMPTY_PAYLOAD_HASH);

    let method = req.method().as_str();
    let path = req.path();
    let query = req.query_string();
    let canonical_uri = path;
    let canonical_query_string = if query.is_empty() {
        String::new()
    } else {
        let mut pairs: Vec<(String, String)> = query
            .split('&')
            .filter_map(|p| {
                let mut it = p.splitn(2, '=');
                let k = it.next()?.to_string();
                let v = it.next().unwrap_or("").to_string();
                Some((percent_decode(&k), percent_decode(&v)))
            })
            .collect();
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
        pairs
            .into_iter()
            .map(|(k, v)| format!("{}={}", percent_encode_uri(&k), percent_encode_uri(&v)))
            .collect::<Vec<_>>()
            .join("&")
    };

    // x-amz-date is required for SigV4 string-to-sign and must be non-empty (trimmed).
    let amz_date_raw = req
        .headers()
        .get("x-amz-date")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ErrorUnauthorized("Missing or invalid x-amz-date"))?;
    let amz_date = amz_date_raw.trim();
    if amz_date.is_empty() {
        return Err(ErrorUnauthorized("Missing or invalid x-amz-date"));
    }
    if !parsed
        .signed_headers
        .iter()
        .any(|h| h == "x-amz-date")
    {
        return Err(ErrorUnauthorized(
            "SigV4 requires x-amz-date in SignedHeaders",
        ));
    }

    // Every header named in Authorization SignedHeaders must be present on the request.
    // Silently omitting missing headers lets clients claim they signed host/date without
    // sending them.
    let mut canonical_headers: Vec<String> = Vec::with_capacity(parsed.signed_headers.len());
    for name in &parsed.signed_headers {
        let value = req
            .headers()
            .get(name.as_str())
            .ok_or_else(|| ErrorUnauthorized("SigV4 SignedHeaders entry not present on request"))?;
        // actix-web's to_str() requires printable ASCII; non-ASCII bytes (e.g. UTF-8 user
        // metadata) return Err even when valid UTF-8. Use from_utf8_lossy so we treat the raw
        // bytes as UTF-8 (same as botocore, which encodes header values as UTF-8 before signing).
        let raw = String::from_utf8_lossy(value.as_bytes());
        let value_str = raw.trim().to_string();
        canonical_headers.push(format!("{}:{}", name, value_str));
    }
    // Sort by header name only (not by full "name:value" string) — important when one header name
    // is a prefix of another (e.g. "x-amz-copy-source" vs "x-amz-copy-source-if-match"): sorting
    // full strings puts them in the wrong order because ':' (58) > '-' (45).
    canonical_headers.sort_unstable_by(|a, b| {
        let a_name = a.split(':').next().unwrap_or("");
        let b_name = b.split(':').next().unwrap_or("");
        a_name.cmp(b_name)
    });
    let canonical_headers_str = canonical_headers.join("\n");
    let signed_headers_str = parsed.signed_headers.join(";");

    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n\n{}\n{}",
        method,
        canonical_uri,
        canonical_query_string,
        canonical_headers_str,
        signed_headers_str,
        payload_hash
    );

    let mut hasher = Sha256::new();
    hasher.update(canonical_request.as_bytes());
    let canonical_request_hash = hex::encode(hasher.finalize());

    let credential_scope = format!(
        "{}/{}/{}/aws4_request",
        parsed.date, parsed.region, parsed.service
    );
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{}\n{}",
        amz_date,
        credential_scope,
        canonical_request_hash
    );

    let k_secret = format!("AWS4{}", secret_key);
    let mut mac = HmacSha256::new_from_slice(k_secret.as_bytes()).map_err(|_| ErrorUnauthorized("HMAC init"))?;
    mac.update(parsed.date.as_bytes());
    let k_date = mac.finalize().into_bytes();

    let mut mac = HmacSha256::new_from_slice(&k_date).map_err(|_| ErrorUnauthorized("HMAC init"))?;
    mac.update(parsed.region.as_bytes());
    let k_region = mac.finalize().into_bytes();

    let mut mac = HmacSha256::new_from_slice(&k_region).map_err(|_| ErrorUnauthorized("HMAC init"))?;
    mac.update(parsed.service.as_bytes());
    let k_service = mac.finalize().into_bytes();

    let mut mac = HmacSha256::new_from_slice(&k_service).map_err(|_| ErrorUnauthorized("HMAC init"))?;
    mac.update(b"aws4_request");
    let k_signing = mac.finalize().into_bytes();

    let mut mac = HmacSha256::new_from_slice(&k_signing).map_err(|_| ErrorUnauthorized("HMAC init"))?;
    mac.update(string_to_sign.as_bytes());

    // SigV4 signatures are hex; accept either upper/lowercase from clients and
    // verify in constant time against the computed HMAC bytes.
    let provided_sig = hex::decode(parsed.signature.trim()).map_err(|_| {
        warn!("SigV4 signature is not valid hex");
        ErrorUnauthorized("Invalid signature format")
    })?;
    if mac.verify_slice(&provided_sig).is_err() {
        warn!(
            "SigV4 signature mismatch (canonical_uri={}, query_len={})",
            canonical_uri,
            canonical_query_string.len()
        );
        return Err(ErrorUnauthorized("Signature does not match"));
    }
    Ok(())
}

/// Percent-decode a string (e.g. "test%2F" -> "test/"). Required so we build the same
/// canonical query string as the client: decode raw query param values then re-encode.
fn percent_decode(s: &str) -> String {
    let mut out = Vec::new();
    let mut bytes = s.bytes();
    while let Some(b) = bytes.next() {
        if b == b'%' {
            let h = bytes.next().and_then(|b| (b as char).to_digit(16));
            let l = bytes.next().and_then(|b| (b as char).to_digit(16));
            if let (Some(h), Some(l)) = (h, l) {
                out.push((h * 16 + l) as u8);
            } else {
                out.push(b'%');
            }
        } else {
            out.push(b);
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn percent_encode_uri(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// True only for AWS ListBuckets: GET/HEAD at the S3 root (no bucket in path).
fn is_list_buckets_request(req: &HttpRequest) -> bool {
    let method = req.method();
    if method != actix_web::http::Method::GET && method != actix_web::http::Method::HEAD {
        return false;
    }
    let p = req.path();
    // /s3 and /s3/ are the warpdrive-prefixed forms; / is the root S3 form (for Ceph s3-tests).
    p == "/s3" || p == "/s3/" || p == "/"
}

/// Extract bucket from request path.
/// Handles both the warpdrive-prefixed form (/s3/{bucket}/...) and the standard
/// root form (/{bucket}/...) that Ceph s3-tests and boto3 clients use by default.
fn extract_bucket_from_path(req: &HttpRequest) -> Result<String, Error> {
    let path = req.path();
    let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();

    // Skip the "s3" prefix if present (warpdrive path-style: /s3/{bucket}/...)
    let bucket_idx = if parts.first().copied() == Some("s3") { 1 } else { 0 };

    if parts.len() <= bucket_idx || parts[bucket_idx].is_empty() {
        return Ok(String::new()); // list-buckets call
    }
    Ok(parts[bucket_idx].to_string())
}

// ---------------------------------------------------------------------------
// Presigned URL support
// ---------------------------------------------------------------------------

fn parse_query_map(req: &HttpRequest) -> HashMap<String, String> {
    req.query_string()
        .split('&')
        .filter_map(|p| {
            let mut it = p.splitn(2, '=');
            let k = percent_decode(it.next()?);
            let v = percent_decode(it.next().unwrap_or(""));
            if k.is_empty() { None } else { Some((k, v)) }
        })
        .collect()
}

struct ParsedPresignedV4 {
    access_key: String,
    date: String,
    region: String,
    service: String,
    amz_date: String,
    expires: u64,
    signed_headers: Vec<String>,
    signature: String,
}

fn parse_presigned_v4(query: &HashMap<String, String>) -> Result<ParsedPresignedV4, Error> {
    let credential = query.get("X-Amz-Credential")
        .ok_or_else(|| ErrorUnauthorized("Missing X-Amz-Credential"))?;
    let (access_key, scope) = credential.split_once('/')
        .ok_or_else(|| ErrorUnauthorized("Invalid X-Amz-Credential"))?;
    let scope_parts: Vec<&str> = scope.splitn(4, '/').collect();
    let date = scope_parts.get(0).ok_or_else(|| ErrorUnauthorized("Invalid credential scope"))?.to_string();
    let region = scope_parts.get(1).unwrap_or(&"us-east-1").to_string();
    let service = scope_parts.get(2).unwrap_or(&"s3").to_string();

    let amz_date = query.get("X-Amz-Date")
        .ok_or_else(|| ErrorUnauthorized("Missing X-Amz-Date"))?.clone();

    let expires_str = query.get("X-Amz-Expires")
        .ok_or_else(|| s3_access_denied("Missing X-Amz-Expires"))?;
    let expires: i64 = expires_str.parse()
        .map_err(|_| s3_access_denied("X-Amz-Expires must be a positive integer"))?;
    if expires <= 0 || expires > 604800 {
        return Err(s3_access_denied("X-Amz-Expires must be between 1 and 604800 seconds"));
    }
    let expires = expires as u64;

    let signed_headers_str = query.get("X-Amz-SignedHeaders")
        .ok_or_else(|| ErrorUnauthorized("Missing X-Amz-SignedHeaders"))?;
    let signed_headers: Vec<String> = signed_headers_str.split(';')
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect();

    let signature = query.get("X-Amz-Signature")
        .ok_or_else(|| ErrorUnauthorized("Missing X-Amz-Signature"))?.clone();

    Ok(ParsedPresignedV4 {
        access_key: access_key.to_string(),
        date,
        region,
        service,
        amz_date,
        expires,
        signed_headers,
        signature,
    })
}

fn check_presigned_expiry(amz_date: &str, expires: u64) -> Result<(), Error> {
    let dt = chrono::NaiveDateTime::parse_from_str(amz_date, "%Y%m%dT%H%M%SZ")
        .map_err(|_| ErrorUnauthorized("Invalid X-Amz-Date format"))?;
    let issued_at = dt.and_utc().timestamp() as u64;
    let expires_at = issued_at + expires;
    let now = chrono::Utc::now().timestamp() as u64;
    if now >= expires_at {
        return Err(s3_access_denied("Request has expired"));
    }
    Ok(())
}

fn verify_sigv4_presigned(req: &HttpRequest, secret_key: &str, parsed: &ParsedPresignedV4) -> Result<(), Error> {
    use hmac::{Hmac, Mac};
    use sha2::{Digest, Sha256};
    type HmacSha256 = Hmac<Sha256>;

    let method = req.method().as_str();
    let canonical_uri = req.path();

    // Canonical query string: all params except X-Amz-Signature, sorted
    let mut pairs: Vec<(String, String)> = req.query_string()
        .split('&')
        .filter_map(|p| {
            let mut it = p.splitn(2, '=');
            let k = it.next()?.to_string();
            let v = it.next().unwrap_or("").to_string();
            Some((percent_decode(&k), percent_decode(&v)))
        })
        .filter(|(k, _)| k != "X-Amz-Signature")
        .collect();
    pairs.sort_by(|a, b| {
        percent_encode_uri(&a.0).cmp(&percent_encode_uri(&b.0))
    });
    let canonical_query_string = pairs.iter()
        .map(|(k, v)| format!("{}={}", percent_encode_uri(k), percent_encode_uri(v)))
        .collect::<Vec<_>>()
        .join("&");

    // Canonical headers: only signed headers (typically just host)
    let mut canonical_headers: Vec<String> = Vec::new();
    for name in &parsed.signed_headers {
        let value = req.headers().get(name.as_str())
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .trim()
            .to_string();
        canonical_headers.push(format!("{}:{}", name, value));
    }
    canonical_headers.sort_unstable_by(|a, b| {
        let a_name = a.split(':').next().unwrap_or("");
        let b_name = b.split(':').next().unwrap_or("");
        a_name.cmp(b_name)
    });
    let canonical_headers_str = canonical_headers.join("\n");
    let signed_headers_str = parsed.signed_headers.join(";");

    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n\n{}\nUNSIGNED-PAYLOAD",
        method, canonical_uri, canonical_query_string,
        canonical_headers_str, signed_headers_str,
    );

    let mut hasher = Sha256::new();
    hasher.update(canonical_request.as_bytes());
    let canonical_request_hash = hex::encode(hasher.finalize());

    let credential_scope = format!("{}/{}/{}/aws4_request", parsed.date, parsed.region, parsed.service);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{}\n{}",
        parsed.amz_date, credential_scope, canonical_request_hash
    );

    let k_secret = format!("AWS4{}", secret_key);
    let mut mac = HmacSha256::new_from_slice(k_secret.as_bytes()).map_err(|_| ErrorUnauthorized("HMAC init"))?;
    mac.update(parsed.date.as_bytes());
    let k_date = mac.finalize().into_bytes();

    let mut mac = HmacSha256::new_from_slice(&k_date).map_err(|_| ErrorUnauthorized("HMAC init"))?;
    mac.update(parsed.region.as_bytes());
    let k_region = mac.finalize().into_bytes();

    let mut mac = HmacSha256::new_from_slice(&k_region).map_err(|_| ErrorUnauthorized("HMAC init"))?;
    mac.update(parsed.service.as_bytes());
    let k_service = mac.finalize().into_bytes();

    let mut mac = HmacSha256::new_from_slice(&k_service).map_err(|_| ErrorUnauthorized("HMAC init"))?;
    mac.update(b"aws4_request");
    let k_signing = mac.finalize().into_bytes();

    let mut mac = HmacSha256::new_from_slice(&k_signing).map_err(|_| ErrorUnauthorized("HMAC init"))?;
    mac.update(string_to_sign.as_bytes());

    let provided_sig = hex::decode(parsed.signature.trim()).map_err(|_| {
        warn!("Presigned V4 signature is not valid hex");
        ErrorUnauthorized("Invalid signature format")
    })?;
    if mac.verify_slice(&provided_sig).is_err() {
        warn!("Presigned V4 signature mismatch (uri={}, query_len={})", canonical_uri, canonical_query_string.len());
        return Err(ErrorUnauthorized("Signature does not match"));
    }
    Ok(())
}

async fn authenticate_presigned_v4(req: &HttpRequest, query: &HashMap<String, String>) -> Result<S3AuthResult, Error> {
    let parsed = parse_presigned_v4(query)?;
    check_presigned_expiry(&parsed.amz_date, parsed.expires)?;

    let bucket = extract_bucket_from_path(req)?;
    let access_key = parsed.access_key.clone();

    // Admin bypass
    let admin_access_key = std::env::var("WARPDRIVE_ADMIN_ACCESS_KEY").ok();
    let admin_secret_key = std::env::var("WARPDRIVE_ADMIN_SECRET_KEY").ok();
    if let (Some(ref aak), Some(ref ask)) = (&admin_access_key, &admin_secret_key) {
        if access_key == *aak {
            verify_sigv4_presigned(req, ask, &parsed)?;
            debug!("Presigned V4 auth: admin bypass OK bucket={:?}", bucket);
            return Ok(S3AuthResult {
                access_key,
                user_id: "admin".to_string(),
                bucket,
                allowed_buckets: vec![],
                allow_all_buckets: true,
            });
        }
    }

    // Console path
    let (base_url, service_secret, cache_ttl_secs) = auth_config_from_env();
    let base_url = base_url.ok_or_else(|| ErrorUnauthorized("No authentication method configured"))?;
    let service_secret = service_secret.ok_or_else(|| ErrorUnauthorized("WARPDRIVE_SERVICE_SECRET not set"))?;
    let (owner_id, secret_key, _, _) =
        load_or_refresh_credential_bundle(&access_key, &base_url, &service_secret, cache_ttl_secs).await?;

    verify_sigv4_presigned(req, &secret_key, &parsed)?;
    debug!("Presigned V4 auth: Console OK bucket={:?} user={}", bucket, owner_id);
    Ok(S3AuthResult {
        access_key,
        user_id: owner_id,
        bucket,
        allowed_buckets: vec![],
        allow_all_buckets: false,
    })
}

/// Authenticate S3 request (async).
///
/// **Admin bypass:** if `WARPDRIVE_ADMIN_ACCESS_KEY` and `WARPDRIVE_ADMIN_SECRET_KEY` are set and
/// the request's access key matches, SigV4 is verified against the admin secret without contacting
/// Vitality Console. The returned result has `allow_all_buckets = true`.
///
/// **Console path:** requires `VITALITY_CONSOLE_URL` + `WARPDRIVE_SERVICE_SECRET`. Credential
/// cache TTL is `S3_AUTH_CACHE_TTL_SECS` (default 300 s).
fn s3_access_denied(message: &str) -> Error {
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <Error>\n\
           <Code>AccessDenied</Code>\n\
           <Message>{}</Message>\n\
           <RequestId>warpdrive</RequestId>\n\
         </Error>",
        message
    );
    let msg = message.to_string();
    actix_web::error::InternalError::from_response(
        msg,
        HttpResponse::Forbidden()
            .content_type("application/xml")
            .insert_header(("x-amz-request-id", "warpdrive"))
            .body(body),
    ).into()
}

pub async fn authenticate_s3_request(req: &HttpRequest) -> Result<S3AuthResult, Error> {
    // Detect presigned requests before looking for Authorization header
    let query_map = parse_query_map(req);
    if query_map.contains_key("X-Amz-Algorithm") {
        return authenticate_presigned_v4(req, &query_map).await;
    }

    let auth_header = req
        .headers()
        .get("Authorization")
        .ok_or_else(|| {
            warn!("Missing Authorization header");
            s3_access_denied("Access Denied")
        })?
        .to_str()
        .map_err(|_| {
            warn!("Invalid Authorization header format");
            s3_access_denied("Access Denied")
        })?;

    let parsed = parse_authorization_header_full(auth_header)?;
    let access_key = parsed.access_key.clone();
    let bucket = extract_bucket_from_path(req)?;

    if !is_list_buckets_request(req) && bucket.is_empty() {
        warn!(
            "S3 auth: empty bucket in path for {} {} — use path-style endpoint so the URL is /s3/{{bucket}}/key",
            req.method(),
            req.path()
        );
        return Err(ErrorBadRequest(
            "S3 path must be /s3/{bucket}/... for this operation (use path-style addressing in your S3 client)",
        ));
    }

    debug!(
        "S3 auth: request_path={} extracted_bucket={:?} list_buckets={}",
        req.path(), bucket, is_list_buckets_request(req)
    );

    // --- Admin user bypass (no Console required) ---
    let admin_access_key = std::env::var("WARPDRIVE_ADMIN_ACCESS_KEY").ok();
    let admin_secret_key = std::env::var("WARPDRIVE_ADMIN_SECRET_KEY").ok();

    if let (Some(ref aak), Some(ref ask)) = (&admin_access_key, &admin_secret_key) {
        if access_key == *aak {
            verify_sigv4(req, ask, &parsed)?;
            debug!("S3 auth: admin bypass OK path_bucket={:?}", bucket);
            return Ok(S3AuthResult {
                access_key,
                user_id: "admin".to_string(),
                bucket,
                allowed_buckets: vec![],
                allow_all_buckets: true,
            });
        }
    }

    // --- Vitality Console path ---
    let (base_url, service_secret, cache_ttl_secs) = auth_config_from_env();

    let base_url = base_url.ok_or_else(|| {
        warn!("VITALITY_CONSOLE_URL not set and no admin credentials configured");
        ErrorUnauthorized("No authentication method configured (set WARPDRIVE_ADMIN_ACCESS_KEY or VITALITY_CONSOLE_URL)")
    })?;
    let service_secret = service_secret.ok_or_else(|| {
        warn!("WARPDRIVE_SERVICE_SECRET not set");
        ErrorUnauthorized("WARPDRIVE_SERVICE_SECRET must be set when using Vitality Console")
    })?;

    let (mut owner_id, mut secret_key, mut allowed_buckets, cache_hit) =
        load_or_refresh_credential_bundle(&access_key, &base_url, &service_secret, cache_ttl_secs).await?;

    if cache_hit && allowed_buckets.is_empty() && !bucket.is_empty() {
        warn!(
            "S3 auth: empty cached bucket set for path_bucket={:?} — invalidating and re-fetching",
            bucket
        );
        invalidate_s3_credential_cache(&access_key);
        let (o, s, ab, _) =
            load_or_refresh_credential_bundle(&access_key, &base_url, &service_secret, cache_ttl_secs).await?;
        owner_id = o;
        secret_key = s;
        allowed_buckets = ab;
    }

    if !bucket.is_empty() && !allowed_buckets.contains(&bucket) {
        warn!(
            "S3 auth: FORBIDDEN path_bucket={:?} not in cached Console bucket set {:?}",
            bucket, allowed_buckets
        );
        return Err(ErrorForbidden(
            "Bucket is not registered for this account in Vitality Console",
        ));
    }

    let mut allowed_buckets_vec: Vec<String> = allowed_buckets.iter().cloned().collect();
    allowed_buckets_vec.sort();

    verify_sigv4(req, &secret_key, &parsed)?;
    debug!(
        "S3 auth: Console path OK request_path={} user={} path_bucket={:?}",
        req.path(), owner_id, bucket
    );
    Ok(S3AuthResult {
        access_key,
        user_id: owner_id,
        bucket,
        allowed_buckets: allowed_buckets_vec,
        allow_all_buckets: false,
    })
}

/// Create a modified request with S3 authentication headers
pub fn create_authenticated_request(req: &HttpRequest, _auth_result: &S3AuthResult) -> HttpRequest {
    req.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::test;

    #[actix_web::test]
    async fn test_parse_authorization_header() {
        let auth_header = "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20231201/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-date, Signature=abc123";
        let (access_key, signature) = parse_authorization_header(auth_header).unwrap();
        assert_eq!(access_key, "AKIAIOSFODNN7EXAMPLE");
        assert_eq!(signature, "abc123");
    }

    #[actix_web::test]
    async fn test_authenticate_s3_request_requires_console_config() {
        std::env::remove_var("VITALITY_CONSOLE_URL");
        std::env::remove_var("WARPDRIVE_SERVICE_SECRET");
        let req = test::TestRequest::default()
            .uri("/s3/test-bucket/test-key")
            .insert_header(("Authorization", "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20231201/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-date, Signature=signature"))
            .to_http_request();
        let result = authenticate_s3_request(&req).await;
        assert!(result.is_err());
    }

    #[actix_web::test]
    async fn test_authenticate_s3_request_errors_when_console_config_missing() {
        std::env::remove_var("VITALITY_CONSOLE_URL");
        let req = test::TestRequest::default()
            .uri("/s3/test-bucket/test-key")
            .insert_header(("Authorization", "AWS4-HMAC-SHA256 Credential=INVALID_KEY/20231201/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-date, Signature=signature"))
            .to_http_request();
        let result = authenticate_s3_request(&req).await;
        assert!(result.is_err());
    }

    #[actix_web::test]
    async fn verify_sigv4_rejects_missing_signed_header() {
        let auth = "AWS4-HMAC-SHA256 Credential=AKIA/20231201/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-date, Signature=deadbeef";
        let parsed = parse_authorization_header_full(auth).unwrap();
        let req = test::TestRequest::default()
            .method(actix_web::http::Method::GET)
            .uri("/s3/b/k")
            .insert_header(("host", "localhost"))
            // x-amz-date listed in SignedHeaders but not sent
            .to_http_request();
        let err = verify_sigv4(&req, "secret", &parsed);
        assert!(err.is_err());
    }

    #[actix_web::test]
    async fn verify_sigv4_rejects_missing_x_amz_date_header() {
        let auth = "AWS4-HMAC-SHA256 Credential=AKIA/20231201/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-date, Signature=deadbeef";
        let parsed = parse_authorization_header_full(auth).unwrap();
        let req = test::TestRequest::default()
            .method(actix_web::http::Method::GET)
            .uri("/s3/b/k")
            .insert_header(("host", "localhost"))
            .to_http_request();
        let err = verify_sigv4(&req, "secret", &parsed);
        assert!(err.is_err());
    }

    #[actix_web::test]
    async fn verify_sigv4_rejects_empty_x_amz_date() {
        let auth = "AWS4-HMAC-SHA256 Credential=AKIA/20231201/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-date, Signature=deadbeef";
        let parsed = parse_authorization_header_full(auth).unwrap();
        let req = test::TestRequest::default()
            .method(actix_web::http::Method::GET)
            .uri("/s3/b/k")
            .insert_header(("host", "localhost"))
            .insert_header(("x-amz-date", "   "))
            .to_http_request();
        let err = verify_sigv4(&req, "secret", &parsed);
        assert!(err.is_err());
    }

    #[actix_web::test]
    async fn verify_sigv4_requires_x_amz_date_in_signed_headers() {
        let auth = "AWS4-HMAC-SHA256 Credential=AKIA/20231201/us-east-1/s3/aws4_request, SignedHeaders=host, Signature=deadbeef";
        let parsed = parse_authorization_header_full(auth).unwrap();
        let req = test::TestRequest::default()
            .method(actix_web::http::Method::GET)
            .uri("/s3/b/k")
            .insert_header(("host", "localhost"))
            .insert_header(("x-amz-date", "20231201T000000Z"))
            .to_http_request();
        let err = verify_sigv4(&req, "secret", &parsed);
        assert!(err.is_err());
    }
}
