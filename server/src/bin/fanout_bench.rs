//! Phase 1 step 7: concurrent vs. sequential fan-out, the one speed claim
//! this phase makes (concurrent latency ≈ max(peer RTT), not sum). Calls
//! `PeerClient::put_shard` directly against N peers, bypassing EC/placement/
//! location-store so only the fan-out strategy itself is measured.
//!
//! Usage: fanout_bench <iterations> <payload_bytes> <peer1> <peer2> ...
//! Example: fanout_bench 30 4096 http://127.0.0.1:9710 http://127.0.0.1:9711 \
//!          http://127.0.0.1:9712 http://127.0.0.1:9713 http://127.0.0.1:9714

use futures::future::join_all;
use std::time::Instant;
use warp_drive::cluster::grpc_peer_client::GrpcPeerClient;
use warp_drive::cluster::peer_client::PeerClient;

fn stats(mut v: Vec<f64>) -> (f64, f64, f64) {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = v.len();
    (v[0], v[n / 2], v[n - 1])
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!("usage: fanout_bench <iterations> <payload_bytes> <peer1> <peer2> ...");
        std::process::exit(1);
    }
    let iters: usize = args[1].parse().unwrap();
    let payload_bytes: usize = args[2].parse().unwrap();
    let peers: Vec<String> = args[3..].to_vec();
    let payload = vec![0xEFu8; payload_bytes];
    let client = GrpcPeerClient::new();

    println!("fanout_bench: iterations={iters} payload_bytes={payload_bytes} peers={}", peers.len());

    let mut concurrent_times = Vec::with_capacity(iters);
    for i in 0..iters {
        let t0 = Instant::now();
        let key = format!("concurrent-{i}");
        let futs = peers.iter().enumerate().map(|(idx, peer)| {
            client.put_shard(peer, "fanoutbucket", &key, idx, payload.clone())
        });
        let results = join_all(futs).await;
        for r in results {
            r.unwrap_or_else(|e| panic!("concurrent put_shard failed: {e}"));
        }
        concurrent_times.push(t0.elapsed().as_secs_f64() * 1000.0);
    }

    let mut sequential_times = Vec::with_capacity(iters);
    for i in 0..iters {
        let t0 = Instant::now();
        for (idx, peer) in peers.iter().enumerate() {
            client
                .put_shard(peer, "fanoutbucket", &format!("sequential-{i}"), idx, payload.clone())
                .await
                .unwrap_or_else(|e| panic!("sequential put_shard failed: {e}"));
        }
        sequential_times.push(t0.elapsed().as_secs_f64() * 1000.0);
    }

    let (cmin, cmed, cmax) = stats(concurrent_times);
    let (smin, smed, smax) = stats(sequential_times);
    println!("concurrent (join_all) ms: min={cmin:.3} median={cmed:.3} max={cmax:.3}");
    println!("sequential (one at a time) ms: min={smin:.3} median={smed:.3} max={smax:.3}");
    println!("speedup (median): {:.2}x", smed / cmed);
}
