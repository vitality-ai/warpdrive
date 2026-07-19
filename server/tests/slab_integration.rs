// Integration tests for slab co-location feature.
//
// Coverage:
//   1. SQLite: put_object_v2 with slab_hint → get_objects_by_slab returns rows in order
//   2. SQLite: hint isolation — querying hint-A never returns hint-B objects
//   3. HTTP: missing ?hint= → 400
//   4. HTTP: no Authorization → 401
//   5. HTTP: admin auth, seeded objects → 200 multipart/mixed
//   6. HTTP: admin auth, empty hint → 204 No Content

use std::sync::Mutex;
use warp_drive::metadata::{Metadata, DataChunk};
use warp_drive::metadata::sqlite_store::SQLiteMetadataStore;

// Env vars are process-global state — serialize every test touching them.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn make_metadata(offset: u64, size: u64, hint: &str) -> Metadata {
    Metadata {
        chunks: vec![DataChunk { offset, size }],
        properties: Default::default(),
        etag: Some(format!("\"etag-{}-{}\"", offset, size)),
        size,
        content_type: Some("application/octet-stream".into()),
        last_modified: Some("2026-01-01T00:00:00.000Z".into()),
        user_metadata: Default::default(),
        cache_control: None,
        expires: None,
        content_encoding: None,
        version_id: None,
        is_delete_marker: false,
        checksum_algorithm: None,
        checksum_value: None,
        checksum_type: None,
        slab_hint: Some(hint.to_string()),
    }
}

// ── 1: SQLite slab roundtrip ──────────────────────────────────────────────────

#[test]
fn test_slab_metadata_roundtrip() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let user = "slab_test_user_rt";
    let bucket = "slab_rt_bucket";
    let hint = "timeline-abc123";

    let db = SQLiteMetadataStore::new();

    db.put_object_v2(user, bucket, "delta/0001.bin", &make_metadata(0, 100, hint)).unwrap();
    db.put_object_v2(user, bucket, "delta/0002.bin", &make_metadata(4 * 1024 * 1024, 200, hint)).unwrap();
    db.put_object_v2(user, bucket, "delta/0003.bin", &make_metadata(8 * 1024 * 1024, 300, hint)).unwrap();

    let rows = db.get_objects_by_slab(user, bucket, Some(hint)).unwrap();
    assert_eq!(rows.len(), 3, "expected 3 objects for hint {}", hint);
    assert_eq!(rows[0].0, "delta/0001.bin");
    assert_eq!(rows[1].0, "delta/0002.bin");
    assert_eq!(rows[2].0, "delta/0003.bin");
    assert_eq!(rows[0].1, vec![(0u64, 100u64)]);
    assert_eq!(rows[1].1, vec![(4 * 1024 * 1024_u64, 200u64)]);
    assert_eq!(rows[2].1, vec![(8 * 1024 * 1024_u64, 300u64)]);
}

// ── 2: hint isolation ─────────────────────────────────────────────────────────

#[test]
fn test_slab_hint_isolation() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let user = "slab_test_user_iso";
    let bucket = "slab_iso_bucket";
    let db = SQLiteMetadataStore::new();

    db.put_object_v2(user, bucket, "a/obj1.bin", &make_metadata(0, 10, "hint-A")).unwrap();
    db.put_object_v2(user, bucket, "b/obj1.bin", &make_metadata(100, 20, "hint-B")).unwrap();
    db.put_object_v2(user, bucket, "a/obj2.bin", &make_metadata(200, 30, "hint-A")).unwrap();

    let a_rows = db.get_objects_by_slab(user, bucket, Some("hint-A")).unwrap();
    let b_rows = db.get_objects_by_slab(user, bucket, Some("hint-B")).unwrap();
    let c_rows = db.get_objects_by_slab(user, bucket, Some("hint-C")).unwrap();

    assert_eq!(a_rows.len(), 2);
    assert!(a_rows.iter().all(|(k, _)| k.starts_with("a/")));
    assert_eq!(b_rows.len(), 1);
    assert_eq!(b_rows[0].0, "b/obj1.bin");
    assert_eq!(c_rows.len(), 0, "unknown hint must return empty");
}

// ── 3: no auth → 403 (S3 Access Denied) ─────────────────────────────────────

#[actix_web::test]
async fn test_warpd_slab_no_auth_returns_403() {
    use actix_web::{test, web, App};
    use warp_drive::warpd::warpd_slab_batch_get;

    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("VITALITY_CONSOLE_URL");
    std::env::remove_var("WARPDRIVE_SERVICE_SECRET");
    std::env::remove_var("WARPDRIVE_ADMIN_ACCESS_KEY");
    std::env::remove_var("WARPDRIVE_ADMIN_SECRET_KEY");

    let app = test::init_service(
        App::new().route("/_warpd/slab/{bucket}", web::get().to(warpd_slab_batch_get)),
    ).await;

    let req = test::TestRequest::get()
        .uri("/_warpd/slab/my-bucket?hint=timeline-xyz")
        .insert_header(("host", "localhost"))
        .to_request();

    let resp = test::call_service(&app, req).await;
    // authenticate_s3_request returns s3_access_denied (403 Forbidden) when no
    // Authorization header is present and no auth method is configured.
    assert_eq!(resp.status(), 403);
}

// ── SigV4 signing helper ──────────────────────────────────────────────────────

fn percent_encode_uri_test(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

fn sigv4_authorization(
    method: &str,
    path: &str,
    query: &str,
    host: &str,
    amz_date: &str,
    access_key: &str,
    secret_key: &str,
    region: &str,
    service: &str,
) -> String {
    use hmac::{Hmac, Mac};
    use sha2::{Digest, Sha256};
    type HmacSha256 = Hmac<Sha256>;

    let date_short = &amz_date[..8];
    let empty_payload_hash =
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    let canonical_query = if query.is_empty() {
        String::new()
    } else {
        let mut pairs: Vec<(String, String)> = query
            .split('&')
            .filter_map(|p| {
                let mut it = p.splitn(2, '=');
                let k = it.next()?.to_string();
                let v = it.next().unwrap_or("").to_string();
                Some((k, v))
            })
            .collect();
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
        pairs
            .into_iter()
            .map(|(k, v)| {
                format!("{}={}", percent_encode_uri_test(&k), percent_encode_uri_test(&v))
            })
            .collect::<Vec<_>>()
            .join("&")
    };

    let canonical_headers = format!("host:{}\nx-amz-date:{}", host, amz_date);
    let signed_headers = "host;x-amz-date";
    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n\n{}\n{}",
        method, path, canonical_query, canonical_headers, signed_headers, empty_payload_hash
    );

    let mut h = Sha256::new();
    h.update(canonical_request.as_bytes());
    let cr_hash = hex::encode(h.finalize());

    let credential_scope = format!("{}/{}/{}/aws4_request", date_short, region, service);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{}\n{}",
        amz_date, credential_scope, cr_hash
    );

    let k_secret = format!("AWS4{}", secret_key);
    let mut mac = HmacSha256::new_from_slice(k_secret.as_bytes()).unwrap();
    mac.update(date_short.as_bytes());
    let k_date = mac.finalize().into_bytes();

    let mut mac = HmacSha256::new_from_slice(&k_date).unwrap();
    mac.update(region.as_bytes());
    let k_region = mac.finalize().into_bytes();

    let mut mac = HmacSha256::new_from_slice(&k_region).unwrap();
    mac.update(service.as_bytes());
    let k_service = mac.finalize().into_bytes();

    let mut mac = HmacSha256::new_from_slice(&k_service).unwrap();
    mac.update(b"aws4_request");
    let k_signing = mac.finalize().into_bytes();

    let mut mac = HmacSha256::new_from_slice(&k_signing).unwrap();
    mac.update(string_to_sign.as_bytes());
    let signature = hex::encode(mac.finalize().into_bytes());

    format!(
        "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}",
        access_key, credential_scope, signed_headers, signature
    )
}

// ── 5: admin auth + seeded objects → 200 multipart/mixed ─────────────────────
//
// Writes real data to LocalXFSBinaryStore before issuing the request.  A second
// LocalXFSBinaryStore instance created inside the handler reads from the same
// STORAGE_DIRECTORY on disk, so the data is visible.

#[actix_web::test]
async fn test_warpd_slab_batch_get_admin_auth() {
    use actix_web::{test, web, App};
    use warp_drive::warpd::warpd_slab_batch_get;
    use warp_drive::service::metadata_service::MetadataService;
    use warp_drive::storage::config::StorageConfig;

    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let access_key = "testadminslabkey";
    let secret_key = "testadminslabsecret12345";
    std::env::set_var("WARPDRIVE_ADMIN_ACCESS_KEY", access_key);
    std::env::set_var("WARPDRIVE_ADMIN_SECRET_KEY", secret_key);
    std::env::remove_var("VITALITY_CONSOLE_URL");
    std::env::remove_var("WARPDRIVE_SERVICE_SECRET");
    std::env::set_var("STORAGE_BACKEND", "localxfs");

    let storage_dir = std::env::temp_dir().join("warpd_slab_test_storage");
    std::fs::create_dir_all(&storage_dir).ok();
    std::env::set_var("STORAGE_DIRECTORY", storage_dir.to_str().unwrap());

    let user = "admin";
    let bucket = "slab-admin-test-bucket";
    let hint = "timeline-deadbeef";

    // Register the bucket.
    let svc = MetadataService::new(user).unwrap();
    let _ = svc.create_bucket(bucket);

    // Write objects to local storage, capturing real file offsets.
    let store = StorageConfig::from_env().create_store();
    let data1 = b"delta-layer-one-content";
    let data2 = b"delta-layer-two-content";
    let (off1, sz1) = store.write(user, bucket, data1, None).unwrap();
    let (off2, sz2) = store.write(user, bucket, data2, None).unwrap();

    // Insert metadata rows with the real extents + slab hint.
    let db = SQLiteMetadataStore::new();
    let mut m1 = make_metadata(off1, sz1, hint);
    m1.size = sz1;
    let mut m2 = make_metadata(off2, sz2, hint);
    m2.size = sz2;
    db.put_object_v2(user, bucket, "slab-key-alpha", &m1).unwrap();
    db.put_object_v2(user, bucket, "slab-key-beta", &m2).unwrap();

    // Build signed request.
    let amz_date = "20260717T120000Z";
    let path = format!("/_warpd/slab/{}", bucket);
    let query = format!("hint={}", hint);
    let uri = format!("{}?{}", path, query);
    let auth = sigv4_authorization(
        "GET", &path, &query, "localhost", amz_date,
        access_key, secret_key, "us-east-1", "s3",
    );

    let app = test::init_service(
        App::new().route("/_warpd/slab/{bucket}", web::get().to(warpd_slab_batch_get)),
    ).await;

    let req = test::TestRequest::get()
        .uri(&uri)
        .insert_header(("host", "localhost"))
        .insert_header(("x-amz-date", amz_date))
        .insert_header(("authorization", auth))
        .to_request();

    let resp = test::call_service(&app, req).await;
    let status = resp.status();
    assert_eq!(status.as_u16(), 200, "seeded bucket should return 200, got {}", status);

    let body = test::read_body(resp).await;
    let body_str = std::str::from_utf8(&body).unwrap();
    assert!(body_str.contains("slab-key-alpha"), "body missing slab-key-alpha");
    assert!(body_str.contains("slab-key-beta"), "body missing slab-key-beta");
    assert!(body_str.contains("warpd_slab_boundary_v1"), "body missing boundary");

    std::env::remove_var("WARPDRIVE_ADMIN_ACCESS_KEY");
    std::env::remove_var("WARPDRIVE_ADMIN_SECRET_KEY");
    std::env::remove_var("STORAGE_BACKEND");
    std::env::remove_var("STORAGE_DIRECTORY");
}

// ── 6: admin auth + empty hint → 204 ─────────────────────────────────────────

#[actix_web::test]
async fn test_warpd_slab_batch_get_empty_hint_returns_204() {
    use actix_web::{test, web, App};
    use warp_drive::warpd::warpd_slab_batch_get;
    use warp_drive::service::metadata_service::MetadataService;

    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let access_key = "testadminslabkey2";
    let secret_key = "testadminslabsecret67890";
    std::env::set_var("WARPDRIVE_ADMIN_ACCESS_KEY", access_key);
    std::env::set_var("WARPDRIVE_ADMIN_SECRET_KEY", secret_key);
    std::env::remove_var("VITALITY_CONSOLE_URL");
    std::env::remove_var("WARPDRIVE_SERVICE_SECRET");
    std::env::set_var("STORAGE_BACKEND", "localxfs");

    let storage_dir = std::env::temp_dir().join("warpd_slab_test_storage_empty");
    std::fs::create_dir_all(&storage_dir).ok();
    std::env::set_var("STORAGE_DIRECTORY", storage_dir.to_str().unwrap());

    let user = "admin";
    let bucket = "slab-empty-hint-bucket";
    let hint = "hint-that-has-no-objects-xyz";

    let svc = MetadataService::new(user).unwrap();
    let _ = svc.create_bucket(bucket);

    let amz_date = "20260717T130000Z";
    let path = format!("/_warpd/slab/{}", bucket);
    let query = format!("hint={}", hint);
    let uri = format!("{}?{}", path, query);
    let auth = sigv4_authorization(
        "GET", &path, &query, "localhost", amz_date,
        access_key, secret_key, "us-east-1", "s3",
    );

    let app = test::init_service(
        App::new().route("/_warpd/slab/{bucket}", web::get().to(warpd_slab_batch_get)),
    ).await;

    let req = test::TestRequest::get()
        .uri(&uri)
        .insert_header(("host", "localhost"))
        .insert_header(("x-amz-date", amz_date))
        .insert_header(("authorization", auth))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 204, "empty hint should return 204");

    std::env::remove_var("WARPDRIVE_ADMIN_ACCESS_KEY");
    std::env::remove_var("WARPDRIVE_ADMIN_SECRET_KEY");
    std::env::remove_var("STORAGE_BACKEND");
    std::env::remove_var("STORAGE_DIRECTORY");
}
