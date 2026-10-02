//! Phase 1 step 4 benchmark: head-to-head single-shard put/get latency,
//! HTTP vs gRPC vs raw-TCP+FlatBuffers `PeerClient`, same peer, same
//! payload size. See docs/Distributed-Engine-Plan.md's transport decision
//! point.
//!
//! Usage: transport_bench <peer_http_base_url> <payload_bytes> <iterations>
//! Example: transport_bench http://127.0.0.1:9710 4096 50
//! (the target node must be running with its HTTP, gRPC, and TCP servers up)

use std::time::Instant;
use warp_drive::cluster::grpc_peer_client::GrpcPeerClient;
use warp_drive::cluster::peer_client::{HttpPeerClient, PeerClient};
use warp_drive::cluster::tcp_peer_client::TcpPeerClient;

fn stats(mut v: Vec<f64>) -> (f64, f64, f64, f64) {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = v.len();
    let min = v[0];
    let max = v[n - 1];
    let median = v[n / 2];
    let mean = v.iter().sum::<f64>() / n as f64;
    (min, median, mean, max)
}

async fn bench_transport(name: &str, client: &dyn PeerClient, peer: &str, bucket: &str, payload: &[u8], iters: usize) {
    let mut put_times = Vec::with_capacity(iters);
    let mut get_times = Vec::with_capacity(iters);

    // Native put_service rejects overwriting an existing key (see
    // docs/Distributed-Engine-Plan.md's known limitation), so each
    // transport/payload-size/run needs its own key namespace.
    let run_id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    for i in 0..iters {
        let key = format!("transport-bench-{name}-{}-{}-{i}", payload.len(), run_id);

        let t0 = Instant::now();
        client
            .put_shard(peer, bucket, &key, 0, payload.to_vec())
            .await
            .unwrap_or_else(|e| panic!("{name} put_shard failed: {e}"));
        put_times.push(t0.elapsed().as_secs_f64() * 1000.0);

        let t0 = Instant::now();
        let got = client
            .get_shard(peer, bucket, &key, 0)
            .await
            .unwrap_or_else(|e| panic!("{name} get_shard failed: {e}"));
        get_times.push(t0.elapsed().as_secs_f64() * 1000.0);

        assert_eq!(got, payload, "{name} round-trip mismatch on iteration {i}");
    }

    let (pmin, pmed, pmean, pmax) = stats(put_times);
    let (gmin, gmed, gmean, gmax) = stats(get_times);
    println!(
        "{name:5} put_shard ms: min={pmin:.3} median={pmed:.3} mean={pmean:.3} max={pmax:.3}"
    );
    println!(
        "{name:5} get_shard ms: min={gmin:.3} median={gmed:.3} mean={gmean:.3} max={gmax:.3}"
    );
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 {
        eprintln!("usage: transport_bench <peer_http_base_url> <payload_bytes> <iterations>");
        std::process::exit(1);
    }
    let peer = &args[1];
    let payload_size: usize = args[2].parse().expect("payload_bytes must be a number");
    let iters: usize = args[3].parse().expect("iterations must be a number");

    let payload = vec![0xABu8; payload_size];
    let bucket = "transport-bench-bucket";

    println!("peer={peer} payload_bytes={payload_size} iterations={iters}");

    let http = HttpPeerClient::new();
    bench_transport("HTTP", &http, peer, bucket, &payload, iters).await;

    let grpc = GrpcPeerClient::new();
    bench_transport("gRPC", &grpc, peer, bucket, &payload, iters).await;

    let tcp = TcpPeerClient::new();
    bench_transport("TCP", &tcp, peer, bucket, &payload, iters).await;
}
