//! Shared-secret authentication for node-to-node cluster traffic: the HTTP
//! `/cluster/*` control and data routes, and the gRPC and raw-TCP shard ports.
//! Every node in a cluster is started with the same `WARPDRIVE_CLUSTER_SECRET`;
//! peers send it with every call and a node rejects anything without it.
//! Single-node servers don't expose any of these, so they never need it.

use lazy_static::lazy_static;
use reqwest::header::{HeaderMap, HeaderValue};

pub const SECRET_ENV: &str = "WARPDRIVE_CLUSTER_SECRET";
/// HTTP header and gRPC metadata key carrying the secret.
pub const SECRET_HEADER: &str = "x-warpdrive-cluster-secret";
const MIN_SECRET_LEN: usize = 16;
/// Upper bound on the TCP handshake frame, read before the peer is trusted.
pub const MAX_HANDSHAKE_LEN: usize = 1024;
/// Reply a TCP shard server sends once the handshake secret matches.
pub const HANDSHAKE_OK: &[u8] = b"ok";

/// Read-only diagnostic counters that stay reachable without the secret
/// (cluster launch scripts poll them as a readiness check).
const OPEN_PATHS: &[&str] = &[
    "/cluster/_internal/timing",
    "/cluster/_internal/shard_timing",
    "/cluster/_internal/grpc_client_timing",
];

lazy_static! {
    static ref SECRET: Option<String> = std::env::var(SECRET_ENV)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
}

pub fn secret() -> Option<&'static str> {
    SECRET.as_deref()
}

/// Called at startup in cluster mode; the server refuses to start on error.
pub fn validate_configured() -> Result<(), String> {
    validate(secret())
}

fn validate(secret: Option<&str>) -> Result<(), String> {
    let s = secret.ok_or_else(|| {
        format!("{SECRET_ENV} must be set (same value on every node) when WARPDRIVE_PEERS or WARPDRIVE_JOIN_VIA is set")
    })?;
    if s.len() < MIN_SECRET_LEN {
        return Err(format!("{SECRET_ENV} must be at least {MIN_SECRET_LEN} characters"));
    }
    if s.len() > MAX_HANDSHAKE_LEN || HeaderValue::from_str(s).is_err() {
        return Err(format!("{SECRET_ENV} must be printable ASCII, at most {MAX_HANDSHAKE_LEN} characters"));
    }
    Ok(())
}

/// True only when a secret is configured and `provided` equals it. Fails closed.
pub fn provided_secret_ok(provided: Option<&[u8]>) -> bool {
    matches(secret(), provided)
}

fn matches(configured: Option<&str>, provided: Option<&[u8]>) -> bool {
    match (configured, provided) {
        (Some(c), Some(p)) => constant_time_eq(c.as_bytes(), p),
        _ => false,
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Whether a request to this path must carry the secret. Everything under
/// `/cluster` does, except the SigV4-authenticated `/cluster/s3` surface and
/// the read-only timing counters. Callers pass both the raw and the decoded
/// path so a percent-encoded spelling can't slip past.
pub fn path_requires_secret(path: &str) -> bool {
    let p = path.trim_end_matches('/');
    if p != "/cluster" && !p.starts_with("/cluster/") {
        return false;
    }
    !(p == "/cluster/s3" || p.starts_with("/cluster/s3/") || OPEN_PATHS.contains(&p))
}

/// Default headers for reqwest clients that call peers.
pub fn peer_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Some(Ok(mut v)) = secret().map(HeaderValue::from_str) {
        v.set_sensitive(true);
        headers.insert(SECRET_HEADER, v);
    }
    headers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_fails_closed() {
        assert!(!matches(None, None));
        assert!(!matches(None, Some(b"anything")));
        assert!(!matches(Some("s3cret-s3cret-s3cret"), None));
        assert!(!matches(Some("s3cret-s3cret-s3cret"), Some(b"s3cret-s3cret-s3creX")));
        assert!(!matches(Some("s3cret-s3cret-s3cret"), Some(b"s3cret")));
        assert!(matches(Some("s3cret-s3cret-s3cret"), Some(b"s3cret-s3cret-s3cret")));
    }

    #[test]
    fn validate_rejects_missing_short_and_non_ascii() {
        assert!(validate(None).is_err());
        assert!(validate(Some("short")).is_err());
        assert!(validate(Some("sixteen-chars-ok\u{7}")).is_err());
        assert!(validate(Some(&"x".repeat(MAX_HANDSHAKE_LEN + 1))).is_err());
        assert!(validate(Some("sixteen-chars-ok")).is_ok());
    }

    #[test]
    fn path_rules() {
        for p in ["/cluster", "/cluster/", "/cluster/join", "/cluster/_internal/location",
                  "/cluster/_admin/bucket_config/b", "/cluster/b/k", "/cluster/b/k/retention",
                  "/cluster/_internal/timing/extra", "/cluster/s3x/b"] {
            assert!(path_requires_secret(p), "{p} should require the secret");
        }
        for p in ["/s3/b/k", "/b/k", "/clusterx/join", "/cluster/s3", "/cluster/s3/b/k",
                  "/cluster/_internal/timing", "/cluster/_internal/shard_timing/"] {
            assert!(!path_requires_secret(p), "{p} should not require the secret");
        }
    }
}
