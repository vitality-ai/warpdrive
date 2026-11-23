//! PUT-then-GET benchmark for the S3 API, signing every request with SigV4
//! in Rust (same signing as load_gen_auth, so client overhead stays low and
//! constant across runs).
//!
//! Phase 1 writes objects for `duration_secs` at the given concurrency.
//! Phase 2 reads those same keys back for `duration_secs`, round-robin.
//! Prints one CSV line per phase:
//!   phase,payload_bytes,concurrency,ops,errors,throughput_ops_s,mean_us,p50_us,p90_us,p99_us
//!
//! Set S3_BENCH_TRACE=<file> to also write one line per request:
//!   phase,start_unix_us,latency_us,ok,key
//!
//! Credentials come from WARPDRIVE_ADMIN_ACCESS_KEY / WARPDRIVE_ADMIN_SECRET_KEY,
//! defaulting to the AWS documentation example pair the test clusters use.
//!
//! Usage: s3_bench <url> <path_prefix> <bucket> <concurrency> <duration_secs> <payload_bytes>
//!   path_prefix: "/s3", "/cluster/s3" or "" for the root routes
//! Example: s3_bench http://127.0.0.1:9710 /s3 bench 32 10 4096

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

type HmacSha256 = Hmac<Sha256>;

fn access_key() -> &'static str {
    static K: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    K.get_or_init(|| std::env::var("WARPDRIVE_ADMIN_ACCESS_KEY").unwrap_or_else(|_| "AKIAIOSFODNN7EXAMPLE".into()))
}

fn secret_key() -> &'static str {
    static K: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    K.get_or_init(|| std::env::var("WARPDRIVE_ADMIN_SECRET_KEY").unwrap_or_else(|_| "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into()))
}
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

/// Headers (including Authorization) for `method path` with this body.
fn sign(method: &str, host: &str, path: &str, body: &[u8]) -> Vec<(String, String)> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    let amz_date = stamp(now.as_secs());
    let date_stamp = &amz_date[..8];
    let payload_hash = hex::encode(Sha256::digest(body));
    let signed_headers = "host;x-amz-content-sha256;x-amz-date";
    let canonical_headers = format!("host:{host}\nx-amz-content-sha256:{payload_hash}\nx-amz-date:{amz_date}\n");
    let canonical_request = format!("{method}\n{path}\n\n{canonical_headers}\n{signed_headers}\n{payload_hash}");
    let scope = format!("{date_stamp}/{REGION}/{SERVICE}/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        hex::encode(Sha256::digest(canonical_request.as_bytes()))
    );
    let signature = hex::encode(hmac_bytes(&signing_key(secret_key(), date_stamp), string_to_sign.as_bytes()));
    vec![
        ("x-amz-content-sha256".into(), payload_hash),
        ("x-amz-date".into(), amz_date.clone()),
        (
            "Authorization".into(),
            format!("AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}", access_key()),
        ),
    ]
}

struct PhaseResult {
    latencies_us: Vec<u64>,
    trace: Vec<(u128, u64, bool, String)>,
    errors: u64,
    error_samples: Vec<String>,
}

#[allow(clippy::too_many_arguments)]
async fn run_phase(
    method: &'static str, url: &str, host: &str, prefix: &str, bucket: &str, keys: Arc<Vec<String>>,
    concurrency: usize, duration: Duration, payload: Arc<Vec<u8>>, write_keys: bool,
) -> (PhaseResult, Vec<String>) {
    let client = reqwest::Client::builder().pool_max_idle_per_host(concurrency).build().unwrap();
    let errors = Arc::new(AtomicU64::new(0));
    let samples: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let stop_at = Instant::now() + duration;
    let run_id = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let mut handles = Vec::with_capacity(concurrency);
    for worker in 0..concurrency {
        let (client, url, host, prefix, bucket) = (client.clone(), url.to_string(), host.to_string(), prefix.to_string(), bucket.to_string());
        let (keys, payload, errors, samples) = (Arc::clone(&keys), Arc::clone(&payload), Arc::clone(&errors), Arc::clone(&samples));
        handles.push(tokio::spawn(async move {
            let mut lat = Vec::new();
            let mut trace = Vec::new();
            let mut written = Vec::new();
            let mut i: usize = 0;
            while Instant::now() < stop_at {
                let key = if write_keys {
                    format!("b-{run_id}-{worker}-{i}")
                } else {
                    if keys.is_empty() { break; }
                    keys[(worker + i * concurrency) % keys.len()].clone()
                };
                let path = format!("{prefix}/{bucket}/{key}");
                let body: &[u8] = if method == "PUT" { &payload } else { b"" };
                let headers = sign(method, &host, &path, body);
                let mut req = match method {
                    "PUT" => client.put(format!("{url}{path}")).body(payload.to_vec()),
                    _ => client.get(format!("{url}{path}")),
                };
                for (k, v) in &headers {
                    req = req.header(k.as_str(), v.as_str());
                }
                let started = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_micros();
                let t0 = Instant::now();
                let res = req.send().await;
                let ok = match res {
                    Ok(resp) if resp.status().is_success() => {
                        // Read the whole body, so GET latency includes the transfer.
                        let n = resp.bytes().await.map(|b| b.len()).unwrap_or(0);
                        method == "PUT" || n == payload.len()
                    }
                    Ok(resp) => {
                        let status = resp.status();
                        let text = resp.text().await.unwrap_or_default();
                        let mut s = samples.lock().unwrap();
                        if s.len() < 5 { s.push(format!("{method} status={status} body={}", &text[..text.len().min(160)])); }
                        false
                    }
                    Err(e) => {
                        let mut s = samples.lock().unwrap();
                        if s.len() < 5 { s.push(format!("{method} transport_error={e}")); }
                        false
                    }
                };
                trace.push((started, t0.elapsed().as_micros() as u64, ok, key.clone()));
                if ok {
                    lat.push(t0.elapsed().as_micros() as u64);
                    if write_keys { written.push(key); }
                } else {
                    errors.fetch_add(1, Ordering::Relaxed);
                }
                i += 1;
            }
            (lat, written, trace)
        }));
    }
    let mut all = Vec::new();
    let mut written = Vec::new();
    let mut trace = Vec::new();
    for h in handles {
        let (lat, w, t) = h.await.unwrap();
        all.extend(lat);
        written.extend(w);
        trace.extend(t);
    }
    let error_samples = samples.lock().unwrap().clone();
    (PhaseResult { latencies_us: all, trace, errors: errors.load(Ordering::Relaxed), error_samples }, written)
}

fn pct(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() { return 0; }
    sorted[(((sorted.len() - 1) as f64) * p).round() as usize]
}

fn report(phase: &str, payload: usize, concurrency: usize, duration: Duration, mut r: PhaseResult) {
    r.latencies_us.sort_unstable();
    let n = r.latencies_us.len();
    let mean = if n == 0 { 0 } else { r.latencies_us.iter().sum::<u64>() / n as u64 };
    println!(
        "{phase},{payload},{concurrency},{n},{},{:.1},{mean},{},{},{}",
        r.errors,
        n as f64 / duration.as_secs_f64(),
        pct(&r.latencies_us, 0.50),
        pct(&r.latencies_us, 0.90),
        pct(&r.latencies_us, 0.99)
    );
    for s in &r.error_samples {
        eprintln!("{phase} error sample: {s}");
    }
    if let Ok(path) = std::env::var("S3_BENCH_TRACE") {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path).expect("trace file");
        for (start, lat, ok, key) in &r.trace {
            writeln!(f, "{phase},{start},{lat},{ok},{key}").unwrap();
        }
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 7 {
        eprintln!("usage: s3_bench <url> <path_prefix> <bucket> <concurrency> <duration_secs> <payload_bytes>");
        std::process::exit(1);
    }
    let url = args[1].trim_end_matches('/').to_string();
    let prefix = args[2].trim_end_matches('/').to_string();
    let bucket = args[3].clone();
    let concurrency: usize = args[4].parse().unwrap();
    let duration = Duration::from_secs(args[5].parse().unwrap());
    let payload_bytes: usize = args[6].parse().unwrap();
    let host = url.trim_start_matches("http://").trim_start_matches("https://").to_string();
    let payload = Arc::new(vec![0xCDu8; payload_bytes]);

    // Create the bucket (ignore "already exists").
    {
        let path = format!("{prefix}/{bucket}");
        let mut req = reqwest::Client::new().put(format!("{url}{path}"));
        for (k, v) in sign("PUT", &host, &path, b"") {
            req = req.header(k, v);
        }
        let _ = req.send().await;
    }

    let (put, written) = run_phase("PUT", &url, &host, &prefix, &bucket, Arc::new(Vec::new()), concurrency, duration, Arc::clone(&payload), true).await;
    report("put", payload_bytes, concurrency, duration, put);
    let (get, _) = run_phase("GET", &url, &host, &prefix, &bucket, Arc::new(written), concurrency, duration, payload, false).await;
    report("get", payload_bytes, concurrency, duration, get);
}

/// UTC "YYYYMMDDTHHMMSSZ" from a unix timestamp (civil-from-days).
fn stamp(unix_secs: u64) -> String {
    let days = unix_secs / 86400;
    let s = unix_secs % 86400;
    let (hh, mm, ss) = (s / 3600, (s % 3600) / 60, s % 60);
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
