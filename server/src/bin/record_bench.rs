//! Micro-benchmark for the per-node record store (`LocationStore`).
//!
//! Measures, on one node's local disk with real fsyncs (group commit):
//!   - durable PUTs per second at 1, 16 and 64 writer threads
//!   - GET latency (p50, p99) on a warm index
//!   - prefix LIST of 1,000 keys
//!
//! Usage: record_bench [dir] [records]
//! Prints one JSON line per measurement.

use std::sync::Arc;
use std::time::Instant;

use warp_drive::cluster::location_store::{BitcaskLocationStore, LocationRecord, LocationStore};

fn record(key: &str, seq: u64) -> LocationRecord {
    LocationRecord {
        user_id: "bench-user".into(),
        bucket: "bench".into(),
        key: key.into(),
        shard_peers: (0..5).map(|i| format!("http://10.0.0.{i}:9710")).collect(),
        k: 3,
        m: 2,
        original_len: 4096,
        version: format!("{seq:x}-0"),
        retention_mode: None,
        retain_until: None,
        legal_hold: false,
        etag: Some("\"d41d8cd98f00b204e9800998ecf8427e\"".into()),
        content_type: Some("application/octet-stream".into()),
        last_modified: Some("Thu, 19 Nov 2026 12:00:00 GMT".into()),
        user_metadata: Default::default(),
        cache_control: None,
        expires: None,
        content_encoding: None,
        checksum_algorithm: Some("CRC32C".into()),
        checksum_value: Some("AAAAAA==".into()),
        checksum_type: Some("FULL_OBJECT".into()),
        tags: Vec::new(),
        inline_data: None,
    }
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    sorted[((sorted.len() as f64 - 1.0) * p) as usize]
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dir = args.get(1).cloned().unwrap_or_else(|| std::env::temp_dir().join("record_bench").to_string_lossy().into_owned());
    let records: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(20_000);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    for threads in [1usize, 16, 64] {
        let path = format!("{dir}/loc-{threads}.log");
        let store = Arc::new(BitcaskLocationStore::open(&path).unwrap());
        let per = records / threads;
        let syncs0 = warp_drive::storage::group_commit::sync_count();
        let t0 = Instant::now();
        let handles: Vec<_> = (0..threads)
            .map(|t| {
                let store = Arc::clone(&store);
                std::thread::spawn(move || {
                    for i in 0..per {
                        let key = format!("obj/{t}/{i}");
                        store.put(record(&key, i as u64)).unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let secs = t0.elapsed().as_secs_f64();
        let syncs = warp_drive::storage::group_commit::sync_count() - syncs0;
        println!(
            "{{\"test\":\"put\",\"threads\":{threads},\"ops\":{},\"ops_per_sec\":{:.0},\"writes_per_sync\":{:.1},\"ms_per_sync\":{:.2}}}",
            per * threads,
            (per * threads) as f64 / secs,
            (per * threads) as f64 / syncs as f64,
            secs * 1000.0 / syncs as f64
        );
    }

    // Reads on the 64-thread store: every key exists.
    let store = BitcaskLocationStore::open(format!("{dir}/loc-64.log")).unwrap();
    let per = records / 64;
    let mut lat = Vec::with_capacity(records);
    let t0 = Instant::now();
    for i in 0..records {
        let key = format!("obj/{}/{}", i % 64, (i / 64) % per);
        let s = Instant::now();
        let r = store.get("bench-user", "bench", &key);
        lat.push(s.elapsed().as_nanos() as f64);
        assert!(r.is_some());
    }
    let secs = t0.elapsed().as_secs_f64();
    lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!("{{\"test\":\"get\",\"ops_per_sec\":{:.0},\"p50_ns\":{:.0},\"p99_ns\":{:.0}}}", records as f64 / secs, pct(&lat, 0.5), pct(&lat, 0.99));

    // Prefix list of one writer's keys.
    let mut lat = Vec::new();
    for t in 0..64 {
        let s = Instant::now();
        let n = store.list("bench-user", "bench", &format!("obj/{t}/")).len();
        lat.push(s.elapsed().as_nanos() as f64);
        assert_eq!(n, per);
    }
    lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!("{{\"test\":\"list\",\"keys\":{per},\"p50_us\":{:.1},\"p99_us\":{:.1}}}", pct(&lat, 0.5) / 1000.0, pct(&lat, 0.99) / 1000.0);

    // Restart: replay the 64-thread log.
    let t0 = Instant::now();
    let _ = BitcaskLocationStore::open(format!("{dir}/loc-64.log")).unwrap();
    println!("{{\"test\":\"replay\",\"records\":{},\"ms\":{:.1}}}", per * 64, t0.elapsed().as_secs_f64() * 1000.0);
}
