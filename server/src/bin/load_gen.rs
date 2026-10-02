//! Sustained concurrent load generator for the cluster PUT path — used to
//! check whether the concurrent-fan-out coordinator actually saturates
//! available cores under load (phase 1 step 7, docs/Distributed-Engine-Plan.md),
//! rather than measuring curl/process-spawn overhead.
//!
//! Usage: load_gen <coordinator_url> <bucket> <concurrency> <duration_secs> <payload_bytes>
//! Example: load_gen http://127.0.0.1:9710 loadbucket 64 5 4096

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 6 {
        eprintln!("usage: load_gen <coordinator_url> <bucket> <concurrency> <duration_secs> <payload_bytes>");
        std::process::exit(1);
    }
    let url = args[1].trim_end_matches('/').to_string();
    let bucket = args[2].clone();
    let concurrency: usize = args[3].parse().unwrap();
    let duration_secs: u64 = args[4].parse().unwrap();
    let payload_bytes: usize = args[5].parse().unwrap();

    let payload = vec![0xCDu8; payload_bytes];
    let client = reqwest::Client::new();
    let completed = Arc::new(AtomicU64::new(0));
    let errors = Arc::new(AtomicU64::new(0));
    let stop_at = Instant::now() + Duration::from_secs(duration_secs);

    println!("load_gen: url={url} bucket={bucket} concurrency={concurrency} duration={duration_secs}s payload_bytes={payload_bytes}");

    // Unique prefix per invocation so repeated runs against the same
    // cluster don't collide with put_service's reject-on-existing-key guard.
    let run_id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();

    let mut handles = Vec::with_capacity(concurrency);
    for worker_id in 0..concurrency {
        let client = client.clone();
        let url = url.clone();
        let bucket = bucket.clone();
        let payload = payload.clone();
        let completed = Arc::clone(&completed);
        let errors = Arc::clone(&errors);
        handles.push(tokio::spawn(async move {
            let mut i: u64 = 0;
            while Instant::now() < stop_at {
                let key = format!("loadgen-{run_id}-{worker_id}-{i}");
                let put_url = format!("{url}/cluster/{bucket}/{key}");
                match client.put(&put_url).body(payload.clone()).send().await {
                    Ok(resp) if resp.status().is_success() => {
                        completed.fetch_add(1, Ordering::Relaxed);
                    }
                    _ => {
                        errors.fetch_add(1, Ordering::Relaxed);
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
}
