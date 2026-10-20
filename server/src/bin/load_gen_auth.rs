// Author: Tejus Chandrashekar
//! Sustained concurrent load generator for the AUTHENTICATED cluster S3
//! path (/cluster/s3/...), signing every request with real SigV4 in Rust
//! -- a fair apples-to-apples comparison against load_gen's bare-path
//! numbers, since a Python/boto3 version of this confounds client-side
//! GIL + signing overhead with any real server-side cost difference (see
//! docs/CHAOS-TESTING.md's perf section, or the commit this file was
//! added in, for the investigation that made this necessary).
//!
//! Usage: load_gen_auth <coordinator_url> <bucket> <concurrency> <duration_secs> <payload_bytes>
//! Example: load_gen_auth http://127.0.0.1:9710 loadbucket 64 10 4096

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

type HmacSha256 = Hmac<Sha256>;

const ACCESS_KEY: &str = "AKIAIOSFODNN7EXAMPLE";
const SECRET_KEY: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
const REGION: &str = "us-east-1";
const SERVICE: &str = "s3";

fn hmac_bytes(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).unwrap();
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn signing_key(secret: &str, date: &str) -> Vec<u8> {
    let k_date = hmac_bytes(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k_region = hmac_bytes(&k_date, REGION.as_bytes());
    let k_service = hmac_bytes(&k_region, SERVICE.as_bytes());
    hmac_bytes(&k_service, b"aws4_request")
}

/// Returns the full set of headers (including Authorization) a PUT to
/// `path` with this body needs, for the given host/timestamp.
fn sign_put(host: &str, path: &str, body: &[u8], amz_date: &str, date_stamp: &str) -> Vec<(String, String)> {
    let payload_hash = hex::encode(Sha256::digest(body));
    let signed_headers = "host;x-amz-content-sha256;x-amz-date";
    let canonical_headers = format!(
        "host:{host}\nx-amz-content-sha256:{payload_hash}\nx-amz-date:{amz_date}\n"
    );
    let canonical_request = format!(
        "PUT\n{path}\n\n{canonical_headers}\n{signed_headers}\n{payload_hash}"
    );
    let canonical_request_hash = hex::encode(Sha256::digest(canonical_request.as_bytes()));
    let credential_scope = format!("{date_stamp}/{REGION}/{SERVICE}/aws4_request");
    let string_to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{credential_scope}\n{canonical_request_hash}");
    let key = signing_key(SECRET_KEY, date_stamp);
    let signature = hex::encode(hmac_bytes(&key, string_to_sign.as_bytes()));
    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={ACCESS_KEY}/{credential_scope}, SignedHeaders={signed_headers}, Signature={signature}"
    );
    vec![
        ("x-amz-content-sha256".to_string(), payload_hash),
        ("x-amz-date".to_string(), amz_date.to_string()),
        ("Authorization".to_string(), authorization),
    ]
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 6 {
        eprintln!("usage: load_gen_auth <coordinator_url> <bucket> <concurrency> <duration_secs> <payload_bytes>");
        std::process::exit(1);
    }
    let url = args[1].trim_end_matches('/').to_string();
    let bucket = args[2].clone();
    let concurrency: usize = args[3].parse().unwrap();
    let duration_secs: u64 = args[4].parse().unwrap();
    let payload_bytes: usize = args[5].parse().unwrap();

    let host = url
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .to_string();
    let payload = vec![0xCDu8; payload_bytes];
    let client = reqwest::Client::new();
    let completed = Arc::new(AtomicU64::new(0));
    let errors = Arc::new(AtomicU64::new(0));
    // Diagnostic only: captures up to 5 distinct failure reasons seen
    // (status + body snippet, or the transport error), so a non-zero error
    // count can actually be explained instead of just reported.
    let error_samples: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let stop_at = Instant::now() + Duration::from_secs(duration_secs);

    println!("load_gen_auth: url={url} bucket={bucket} concurrency={concurrency} duration={duration_secs}s payload_bytes={payload_bytes}");

    // Ensure the bucket exists (implicit on cluster anyway, but a real
    // client would've created it).
    {
        let put_bucket_path = format!("/cluster/s3/{bucket}");
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        let amz_date = chrono_stamp(now.as_secs());
        let date_stamp = amz_date[..8].to_string();
        let headers = sign_put(&host, &put_bucket_path, b"", &amz_date, &date_stamp);
        let mut req = client.put(format!("{url}{put_bucket_path}")).body(Vec::new());
        for (k, v) in &headers {
            req = req.header(k.as_str(), v.as_str());
        }
        let _ = req.send().await;
    }

    let run_id = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();

    let mut handles = Vec::with_capacity(concurrency);
    for worker_id in 0..concurrency {
        let client = client.clone();
        let url = url.clone();
        let bucket = bucket.clone();
        let payload = payload.clone();
        let host = host.clone();
        let completed = Arc::clone(&completed);
        let errors = Arc::clone(&errors);
        let error_samples = Arc::clone(&error_samples);
        handles.push(tokio::spawn(async move {
            let mut i: u64 = 0;
            while Instant::now() < stop_at {
                let key = format!("loadgenauth-{run_id}-{worker_id}-{i}");
                let path = format!("/cluster/s3/{bucket}/{key}");
                let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
                let amz_date = chrono_stamp(now.as_secs());
                let date_stamp = amz_date[..8].to_string();
                let headers = sign_put(&host, &path, &payload, &amz_date, &date_stamp);
                let mut req = client.put(format!("{url}{path}")).body(payload.clone());
                for (k, v) in &headers {
                    req = req.header(k.as_str(), v.as_str());
                }
                match req.send().await {
                    Ok(resp) if resp.status().is_success() => {
                        completed.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(resp) => {
                        errors.fetch_add(1, Ordering::Relaxed);
                        let status = resp.status();
                        let body = resp.text().await.unwrap_or_default();
                        let mut samples = error_samples.lock().unwrap();
                        if samples.len() < 5 {
                            samples.push(format!("status={status} body={}", &body[..body.len().min(200)]));
                        }
                    }
                    Err(e) => {
                        errors.fetch_add(1, Ordering::Relaxed);
                        let mut samples = error_samples.lock().unwrap();
                        if samples.len() < 5 {
                            samples.push(format!("transport_error={e}"));
                        }
                    }
                }
                i += 1;
            }
        }));
    }

    for h in handles {
        let _ = h.await;
    }

    let total = completed.load(Ordering::Relaxed);
    let errs = errors.load(Ordering::Relaxed);
    println!(
        "completed={total} errors={errs} throughput={:.1} req/s",
        total as f64 / duration_secs as f64
    );
    for (i, sample) in error_samples.lock().unwrap().iter().enumerate() {
        println!("error_sample[{i}]: {sample}");
    }
}

/// Minimal UTC "YYYYMMDDTHHMMSSZ" stamp from a unix timestamp, without
/// pulling in chrono for just this (this bin has no other date need).
fn chrono_stamp(unix_secs: u64) -> String {
    let days = unix_secs / 86400;
    let secs_of_day = unix_secs % 86400;
    let (hh, mm, ss) = (secs_of_day / 3600, (secs_of_day % 3600) / 60, secs_of_day % 60);

    // Civil-from-days (Howard Hinnant's algorithm), proleptic Gregorian.
    let z = days as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!("{y:04}{m:02}{d:02}T{hh:02}{mm:02}{ss:02}Z")
}
