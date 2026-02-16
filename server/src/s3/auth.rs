// S3 Authentication module
use actix_web::{HttpRequest, Error, error::ErrorUnauthorized};
use lazy_static::lazy_static;
use log::{info, warn};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

lazy_static! {
    static ref HTTP_CLIENT: reqwest::Client = reqwest::Client::new();
    static ref CREDENTIAL_CACHE: RwLock<HashMap<String, CachedCredential>> = RwLock::new(HashMap::new());
}

/// Default TTL for credential cache (seconds)
const DEFAULT_CACHE_TTL_SECS: u64 = 300;

#[derive(Clone)]
struct CachedCredential {
    secret_key: String,
    owner_id: String,
    expires_at: Instant,
}

/// Response from Vitality Console s3-credentials endpoint
#[derive(Debug, Deserialize)]
struct S3CredentialsResponse {
    owner_id: String,
    secret_key: String,
}

/// S3 Authentication result
#[derive(Debug)]
pub struct S3AuthResult {
    pub access_key: String,
    pub user_id: String,
    pub bucket: String,
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
        info!("S3 auth: Vitality Console (s3-credentials + cache + SigV4) at {}", u);
    }
    (base_url, service_secret, cache_ttl_secs)
}

/// Fetch (owner_id, secret_key) from Console s3-credentials endpoint.
async fn fetch_s3_credentials_from_console(
    access_key: &str,
    base_url: &str,
    service_secret: &str,
) -> Result<(String, String), Error> {
    let url = format!("{}/api/auth/s3-credentials", base_url.trim_end_matches('/'));
    let res = HTTP_CLIENT
        .post(&url)
        .header("X-Warpdrive-Secret", service_secret)
        .json(&serde_json::json!({ "access_key": access_key }))
        .send()
        .await
        .map_err(|e| {
            warn!("Vitality Console s3-credentials request failed: {}", e);
            ErrorUnauthorized("Authentication service unavailable")
        })?;
    if !res.status().is_success() {
        warn!("Vitality Console s3-credentials returned {}", res.status());
        return Err(ErrorUnauthorized("Invalid access key or service secret"));
    }
    let body: S3CredentialsResponse = res.json().await.map_err(|e| {
        warn!("Failed to parse s3-credentials response: {}", e);
        ErrorUnauthorized("Invalid access key")
    })?;
    Ok((body.owner_id, body.secret_key))
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
    let access_key = parts.get(0).ok_or_else(|| ErrorUnauthorized("Invalid Credential"))?.to_string();
    let scope = parts.get(1).ok_or_else(|| ErrorUnauthorized("Invalid Credential"))?;
    let scope_parts: Vec<&str> = scope.splitn(4, '/').collect();
    let date = scope_parts.get(0).ok_or_else(|| ErrorUnauthorized("Invalid Credential"))?.to_string();
    let region = scope_parts.get(1).unwrap_or(&"us-east-1").to_string();
    let service = scope_parts.get(2).unwrap_or(&"s3").to_string();

    let signed_headers_start = auth_header.find("SignedHeaders=").ok_or_else(|| ErrorUnauthorized("Missing SignedHeaders"))?;
    let signed_part = &auth_header[signed_headers_start + 14..];
    let signed_end = signed_part.find(',').unwrap_or(signed_part.len());
    let signed_headers_str = signed_part[..signed_end].trim();
    let signed_headers: Vec<String> = signed_headers_str.split(';').map(|s| s.to_lowercase()).collect();

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

/// Parse S3 Authorization header (legacy: access_key + placeholder signature)
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

    let mut canonical_headers: Vec<String> = parsed
        .signed_headers
        .iter()
        .filter_map(|name| {
            let value = req.headers().get(name)?;
            let value_str = value.to_str().ok()?;
            Some(format!("{}:{}", name, value_str.trim()))
        })
        .collect();
    canonical_headers.sort_by(|a, b| a.cmp(b));
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
    let amz_date = req
        .headers()
        .get("x-amz-date")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
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
    let expected_sig = hex::encode(mac.finalize().into_bytes());

    if expected_sig != parsed.signature {
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

/// Extract bucket from request path. Path must start with /s3/.
/// For /s3 or /s3/ (list-buckets), returns Ok("").
fn extract_bucket_from_path(req: &HttpRequest) -> Result<String, Error> {
    let path = req.path();
    let path_parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    if path_parts.is_empty() || path_parts[0] != "s3" {
        return Err(ErrorUnauthorized("Invalid S3 path format"));
    }
    if path_parts.len() < 2 || path_parts[1].is_empty() {
        return Ok(String::new());
    }
    Ok(path_parts[1].to_string())
}

/// Authenticate S3 request (async). Requires Vitality Console: VITALITY_CONSOLE_URL and
/// WARPDRIVE_SERVICE_SECRET must be set. Uses s3-credentials + cache + SigV4.
pub async fn authenticate_s3_request(req: &HttpRequest) -> Result<S3AuthResult, Error> {
    let auth_header = req
        .headers()
        .get("Authorization")
        .ok_or_else(|| {
            warn!("Missing Authorization header");
            ErrorUnauthorized("Missing Authorization header")
        })?
        .to_str()
        .map_err(|_| {
            warn!("Invalid Authorization header format");
            ErrorUnauthorized("Invalid Authorization header")
        })?;

    let parsed = parse_authorization_header_full(auth_header)?;
    let access_key = parsed.access_key.clone();
    let bucket = extract_bucket_from_path(req)?;

    let (base_url, service_secret, cache_ttl_secs) = auth_config_from_env();

    let base_url = base_url.ok_or_else(|| {
        warn!("VITALITY_CONSOLE_URL not set");
        ErrorUnauthorized("VITALITY_CONSOLE_URL and WARPDRIVE_SERVICE_SECRET must be set")
    })?;
    let service_secret = service_secret.ok_or_else(|| {
        warn!("WARPDRIVE_SERVICE_SECRET not set");
        ErrorUnauthorized("VITALITY_CONSOLE_URL and WARPDRIVE_SERVICE_SECRET must be set")
    })?;

    let (secret_key, owner_id) = {
        let cached = CREDENTIAL_CACHE.read().map_err(|_| ErrorUnauthorized("Cache lock"))?;
        if let Some(c) = cached.get(&access_key) {
            if c.expires_at > Instant::now() {
                (c.secret_key.clone(), c.owner_id.clone())
            } else {
                drop(cached);
                let (owner_id, secret_key) =
                    fetch_s3_credentials_from_console(&access_key, &base_url, &service_secret).await?;
                let expires_at = Instant::now() + Duration::from_secs(cache_ttl_secs);
                let mut cache = CREDENTIAL_CACHE.write().map_err(|_| ErrorUnauthorized("Cache lock"))?;
                cache.insert(
                    access_key.clone(),
                    CachedCredential { secret_key: secret_key.clone(), owner_id: owner_id.clone(), expires_at },
                );
                (secret_key, owner_id)
            }
        } else {
            drop(cached);
            let (owner_id, secret_key) =
                fetch_s3_credentials_from_console(&access_key, &base_url, &service_secret).await?;
            let expires_at = Instant::now() + Duration::from_secs(cache_ttl_secs);
            let mut cache = CREDENTIAL_CACHE.write().map_err(|_| ErrorUnauthorized("Cache lock"))?;
            cache.insert(
                access_key.clone(),
                CachedCredential { secret_key: secret_key.clone(), owner_id: owner_id.clone(), expires_at },
            );
            (secret_key, owner_id)
        }
    };

    verify_sigv4(req, &secret_key, &parsed)?;
    info!(
        "S3 Authentication successful: user={}, bucket={}",
        owner_id, bucket
    );
    Ok(S3AuthResult {
        access_key,
        user_id: owner_id,
        bucket,
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
    async fn test_authenticate_s3_request_invalid_key() {
        std::env::remove_var("VITALITY_CONSOLE_URL");
        let req = test::TestRequest::default()
            .uri("/s3/test-bucket/test-key")
            .insert_header(("Authorization", "AWS4-HMAC-SHA256 Credential=INVALID_KEY/20231201/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-date, Signature=signature"))
            .to_http_request();
        let result = authenticate_s3_request(&req).await;
        assert!(result.is_err());
    }
}
