//! gRPC `PeerClient` implementation — the alternative to `HttpPeerClient`
//! in the transport decision point (docs/Distributed-Engine-Plan.md). The
//! server side (`ShardServiceImpl`) calls `shard_storage` directly, so
//! shards written via gRPC are stored identically to shards written via
//! the HTTP path's native API handlers — either `PeerClient` impl can read
//! what the other wrote.

use actix_web::error::ErrorBadGateway;
use actix_web::Error;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::OnceCell;
use tonic::transport::{Channel, Endpoint};
use tonic::{Request, Response, Status};

use super::peer_client::PeerClient;
use super::shard_proto::shard_service_client::ShardServiceClient;
use super::shard_proto::shard_service_server::{ShardService, ShardServiceServer};
use super::shard_proto::{GetShardRequest, GetShardResponse, PutShardRequest, PutShardResponse};
use super::shard_storage::{load_shard, store_shard};
use super::timing_stats::Phase;
use lazy_static::lazy_static;

/// Diagnostic: splits `put_shard`'s client-side latency into "getting a
/// channel" (cache lookup, or connect on first use) vs. "the RPC call
/// itself" (send + server processing + receive) — added because
/// server-side timing showed the actual handler takes ~0.07ms, nowhere
/// near the ~10ms the coordinator sees, so the gap has to be in this
/// client/transport layer somewhere. See docs/Distributed-Engine-Plan.md.
#[derive(Default)]
pub struct ClientTiming {
    pub channel_lookup: Phase,
    pub rpc_call: Phase,
}

impl ClientTiming {
    pub fn summary(&self) -> String {
        format!(
            "channel_lookup_avg_ms={:.3} channel_lookup_max_ms={:.3}\nrpc_call_avg_ms={:.3} rpc_call_max_ms={:.3}\n",
            self.channel_lookup.avg_ms(),
            self.channel_lookup.max_ms(),
            self.rpc_call.avg_ms(),
            self.rpc_call.max_ms(),
        )
    }
}

lazy_static! {
    pub static ref CLIENT_TIMING: ClientTiming = ClientTiming::default();
}

// ---------------------------------------------------------------------------
// Server side
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct ShardServiceImpl;

#[tonic::async_trait]
impl ShardService for ShardServiceImpl {
    async fn put_shard(&self, request: Request<PutShardRequest>) -> Result<Response<PutShardResponse>, Status> {
        let req = request.into_inner();
        store_shard(&req.bucket, &req.key, req.shard_idx as usize, &req.data)
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(PutShardResponse {}))
    }

    async fn get_shard(&self, request: Request<GetShardRequest>) -> Result<Response<GetShardResponse>, Status> {
        let req = request.into_inner();
        let data = load_shard(&req.bucket, &req.key, req.shard_idx as usize)
            .map_err(|e| Status::not_found(e.to_string()))?;
        Ok(Response::new(GetShardResponse { data }))
    }
}

pub fn make_server() -> ShardServiceServer<ShardServiceImpl> {
    ShardServiceServer::new(ShardServiceImpl)
}

// ---------------------------------------------------------------------------
// Client side
// ---------------------------------------------------------------------------

/// `peer` addresses passed to `PeerClient` methods are HTTP base URLs
/// (`http://host:port`) shared with `HttpPeerClient`/`Membership` — this
/// client derives the gRPC endpoint from the same address by applying a
/// fixed port offset, so callers don't need a second peer-list format.
pub const GRPC_PORT_OFFSET: u16 = 1000;

fn grpc_endpoint_for(peer: &str) -> Result<String, Error> {
    let stripped = peer.trim_end_matches('/');
    let (scheme_host, port_str) = stripped.rsplit_once(':').ok_or_else(|| ErrorBadGateway("peer address missing port"))?;
    let port: u16 = port_str.parse().map_err(|_| ErrorBadGateway("peer address has a non-numeric port"))?;
    Ok(format!("{scheme_host}:{}", port + GRPC_PORT_OFFSET))
}

/// A fixed-size pool of `Channel`s (separate HTTP/2 connections) to one
/// peer, picked round-robin. Diagnosis (see docs/Distributed-Engine-Plan.md):
/// a single shared `Channel` meant every connection's own h2 driver task —
/// inherently one task, serial by construction — had to multiplex every
/// concurrent request to that peer. Confirmed by timing: server-side
/// handler work was ~0.07ms, but the full RPC call averaged ~6.8ms under
/// 64-way concurrent load, with channel *lookup* itself negligible
/// (~0.007ms) — the gap was the single connection's multiplexing, not the
/// cache. Multiple connections spread that multiplexing work across
/// multiple driver tasks (and, in a multi-threaded runtime, multiple OS
/// threads) instead of funneling everything through one.
struct PeerChannels {
    channels: Vec<Channel>,
    next: std::sync::atomic::AtomicUsize,
}

impl PeerChannels {
    fn pick(&self) -> Channel {
        let idx = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % self.channels.len();
        self.channels[idx].clone()
    }
}

pub struct GrpcPeerClient {
    // One `OnceCell` per endpoint, so exactly one caller ever runs the
    // connect loop below; every other concurrent caller awaits that same
    // in-progress initialization instead of starting its own. A first,
    // broken version of this used a plain `Mutex<HashMap<_, Arc<_>>>` with
    // a check-then-build pattern: under real concurrent load (256 clients,
    // 5 peers each) many callers simultaneously saw "no pool yet" and each
    // independently opened `pool_size` connections — thousands of
    // simultaneous connection attempts, confirmed on a GCP VM to collapse
    // throughput from ~2700 req/s to ~58 req/s with growing errors. This
    // is the actual fix, not a hypothetical one.
    pools: Mutex<HashMap<String, Arc<OnceCell<Arc<PeerChannels>>>>>,
    pool_size: usize,
}

impl GrpcPeerClient {
    pub fn new() -> Self {
        let pool_size = std::env::var("WARPDRIVE_GRPC_POOL_SIZE")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&n: &usize| n > 0)
            .unwrap_or(8);
        Self {
            pools: Mutex::new(HashMap::new()),
            pool_size,
        }
    }

    async fn client_for(&self, peer: &str) -> Result<ShardServiceClient<Channel>, Error> {
        let endpoint = grpc_endpoint_for(peer)?;

        let cell = {
            let mut pools = self.pools.lock().unwrap();
            Arc::clone(
                pools
                    .entry(endpoint.clone())
                    .or_insert_with(|| Arc::new(OnceCell::new())),
            )
        };

        let pool_size = self.pool_size;
        let endpoint_for_init = endpoint.clone();
        let pool = cell
            .get_or_try_init(|| async move {
                let mut channels = Vec::with_capacity(pool_size);
                for _ in 0..pool_size {
                    let endpoint_obj = Endpoint::from_shared(endpoint_for_init.clone())
                        .map_err(|e| ErrorBadGateway(format!("invalid gRPC endpoint {endpoint_for_init}: {e}")))?;
                    let connect_result = endpoint_obj.connect().await;
                    let channel = connect_result
                        .map_err(|e| ErrorBadGateway(format!("gRPC connect to {endpoint_for_init} failed: {e}")))?;
                    channels.push(channel);
                }
                Ok::<_, Error>(Arc::new(PeerChannels {
                    channels,
                    next: std::sync::atomic::AtomicUsize::new(0),
                }))
            })
            .await?;

        Ok(ShardServiceClient::new(pool.pick()))
    }
}

impl Default for GrpcPeerClient {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl PeerClient for GrpcPeerClient {
    async fn put_shard(&self, peer: &str, bucket: &str, key: &str, shard_idx: usize, data: Vec<u8>) -> Result<(), Error> {
        let t0 = std::time::Instant::now();
        let mut client = self.client_for(peer).await?;
        CLIENT_TIMING.channel_lookup.record(t0.elapsed());

        let t0 = std::time::Instant::now();
        let result = client
            .put_shard(PutShardRequest {
                bucket: bucket.to_string(),
                key: key.to_string(),
                shard_idx: shard_idx as u32,
                data,
            })
            .await;
        CLIENT_TIMING.rpc_call.record(t0.elapsed());
        result.map_err(|e| ErrorBadGateway(format!("put_shard to {peer} failed: {e}")))?;
        Ok(())
    }

    async fn get_shard(&self, peer: &str, bucket: &str, key: &str, shard_idx: usize) -> Result<Vec<u8>, Error> {
        let mut client = self.client_for(peer).await?;
        let resp = client
            .get_shard(GetShardRequest {
                bucket: bucket.to_string(),
                key: key.to_string(),
                shard_idx: shard_idx as u32,
            })
            .await
            .map_err(|e| ErrorBadGateway(format!("get_shard from {peer} failed: {e}")))?;
        Ok(resp.into_inner().data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grpc_endpoint_applies_fixed_port_offset() {
        assert_eq!(
            grpc_endpoint_for("http://127.0.0.1:9710").unwrap(),
            "http://127.0.0.1:10710"
        );
        assert_eq!(
            grpc_endpoint_for("http://127.0.0.1:9710/").unwrap(),
            "http://127.0.0.1:10710"
        );
    }
}
