//! Raw-TCP `PeerClient` implementation — the third transport candidate in
//! the decision point (docs/Distributed-Engine-Plan.md), built specifically
//! to test the diagnosis that HTTP/2's own framing/multiplexing (not
//! serialization cost) was the remaining overhead in the gRPC transport.
//! No HTTP/2, no HPACK, no stream flow control: just a TCP connection, a
//! 4-byte big-endian length prefix, and a FlatBuffers-encoded `Envelope`
//! (`ShardWire.fbs` / `shard_wire_generated.rs`).
//!
//! Where gRPC gets concurrency via multiplexing many streams over few
//! connections, this gets it via a pool of plain TCP connections, checked
//! out for one request/response round trip and returned — concurrency
//! comes from pool size, not multiplexing a shared connection.
//!
//! **Decision, not just a benchmark result: gRPC stays the default.**
//! Per-call latency (no concurrent load) favors this transport — see
//! `transport_bench`. But under real concurrent load, measured on GCP,
//! this transport is ~8-9x slower than gRPC at a pool size that's actually
//! safe for file descriptors (see `PeerPool`'s doc comment for the full
//! story and the formula). That's structural, not a tuning problem: no
//! multiplexing means no way to be both fast and fd-safe at once. Kept in
//! the tree, fully correct and tested, as a documented "no" with real
//! numbers behind it — set `WARPDRIVE_PEER_TRANSPORT=tcp` to use it anyway.

use actix_web::error::ErrorBadGateway;
use actix_web::Error;
use async_trait::async_trait;
use flatbuffers::FlatBufferBuilder;
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex as StdMutex;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex as AsyncMutex, OwnedSemaphorePermit, Semaphore};

use super::peer_client::PeerClient;
use super::shard_storage::{load_shard, store_shard};
use super::shard_wire_generated::shardwire::{
    root_as_envelope, Body, Envelope, EnvelopeArgs, GetShardRequest, GetShardRequestArgs,
    GetShardResponse, GetShardResponseArgs, PutShardRequest, PutShardRequestArgs,
    PutShardResponse, PutShardResponseArgs,
};

/// Same peer-address-derivation trick as `GRPC_PORT_OFFSET`, a different
/// offset so the two transports never collide on the same port.
pub const TCP_PORT_OFFSET: u16 = 2000;

fn tcp_endpoint_for(peer: &str) -> Result<String, Error> {
    let stripped = peer.trim_end_matches('/');
    let (scheme_host, port_str) = stripped
        .rsplit_once(':')
        .ok_or_else(|| ErrorBadGateway("peer address missing port"))?;
    let host = scheme_host.rsplit_once("//").map(|(_, h)| h).unwrap_or(scheme_host);
    let port: u16 = port_str
        .parse()
        .map_err(|_| ErrorBadGateway("peer address has a non-numeric port"))?;
    Ok(format!("{host}:{}", port + TCP_PORT_OFFSET))
}

// ---------------------------------------------------------------------------
// Wire framing: 4-byte big-endian length prefix + FlatBuffers Envelope bytes
// ---------------------------------------------------------------------------

async fn write_framed(stream: &mut TcpStream, bytes: &[u8]) -> std::io::Result<()> {
    stream.write_all(&(bytes.len() as u32).to_be_bytes()).await?;
    stream.write_all(bytes).await?;
    stream.flush().await
}

async fn read_framed(stream: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;
    Ok(buf)
}

// ---------------------------------------------------------------------------
// Client side: a small per-peer pool of plain TCP connections
// ---------------------------------------------------------------------------

/// **History, corrected twice on real infrastructure — read before changing
/// `pool_size`.** An unbounded version (connect-on-demand, no cap) measured
/// faster on Mac, but failed outright on a GCP VM under the same load with
/// `Too many open files (os error 24)`: Mac's default file descriptor
/// limit is generous enough to never expose unbounded growth; a stock
/// Linux `ulimit -n` (commonly 1024) is not. A bound is a correctness
/// requirement, not optional defensive complexity.
///
/// The first bounded attempt used 128 per peer — still failed the same way
/// at concurrency=256 on a 5-node cluster, because TCP has no
/// multiplexing: `peers × pool_size × 2` (outbound + the same inbound load
/// from peers dialing in) has to fit under `ulimit -n`, with headroom left
/// for the gRPC/HTTP servers, SQLite, log files, etc. that are always
/// running too. `128 × 4 peers × 2 ≈ 1024` — exactly the limit, no margin.
/// Default here is 32 (`32 × 4 × 2 = 256`, comfortable headroom), safe but
/// **measured ~8-9x slower than gRPC at this size** on GCP (781-803 req/s
/// vs. gRPC's 6841-7005 req/s, same VM, same session) — this is the real,
/// structural cost of no multiplexing: gRPC carries hundreds of concurrent
/// logical requests over a handful of HTTP/2 connections; raw TCP needs
/// one connection per in-flight request, so its safe concurrency ceiling
/// is directly bounded by file descriptors, not sized by preference.
/// Raising this value trades fd-exhaustion risk for throughput — there is
/// no value that is both safe and fast without multiplexing, which is
/// exactly the complexity this transport was built to avoid. Formula for
/// tuning: keep `peer_count × pool_size × 2` safely under this process's
/// `ulimit -n` minus everything else it needs.
struct PeerPool {
    idle: AsyncMutex<VecDeque<TcpStream>>,
    limit: Arc<Semaphore>,
}

pub struct TcpPeerClient {
    pools: StdMutex<HashMap<String, Arc<PeerPool>>>,
    pool_size: usize,
}

impl TcpPeerClient {
    pub fn new() -> Self {
        let pool_size = std::env::var("WARPDRIVE_TCP_POOL_SIZE")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&n: &usize| n > 0)
            .unwrap_or(32);
        Self {
            pools: StdMutex::new(HashMap::new()),
            pool_size,
        }
    }

    fn pool_for(&self, endpoint: &str) -> Arc<PeerPool> {
        let mut pools = self.pools.lock().unwrap();
        let pool_size = self.pool_size;
        Arc::clone(pools.entry(endpoint.to_string()).or_insert_with(|| {
            Arc::new(PeerPool {
                idle: AsyncMutex::new(VecDeque::new()),
                limit: Arc::new(Semaphore::new(pool_size)),
            })
        }))
    }

    async fn borrow(&self, endpoint: &str) -> Result<(TcpStream, OwnedSemaphorePermit), Error> {
        let pool = self.pool_for(endpoint);
        // Blocks here once `pool_size` connections for this peer are all
        // in use, instead of opening an unbounded number of new ones.
        let permit = Arc::clone(&pool.limit)
            .acquire_owned()
            .await
            .map_err(|e| ErrorBadGateway(format!("tcp pool semaphore for {endpoint} closed: {e}")))?;

        let idle_stream = pool.idle.lock().await.pop_front();
        let stream = match idle_stream {
            Some(s) => s,
            None => TcpStream::connect(endpoint)
                .await
                .map_err(|e| ErrorBadGateway(format!("tcp connect to {endpoint} failed: {e}")))?,
        };
        Ok((stream, permit))
    }

    async fn release(&self, endpoint: &str, stream: TcpStream, permit: OwnedSemaphorePermit) {
        let pool = self.pool_for(endpoint);
        pool.idle.lock().await.push_back(stream);
        drop(permit);
    }

    async fn roundtrip(&self, peer: &str, request: &[u8]) -> Result<Vec<u8>, Error> {
        let endpoint = tcp_endpoint_for(peer)?;
        let (mut stream, permit) = self.borrow(&endpoint).await?;

        if let Err(e) = write_framed(&mut stream, request).await {
            // Don't return a broken connection to the pool; let it (and
            // its permit) drop, freeing the slot for a fresh connection.
            return Err(ErrorBadGateway(format!("tcp write to {peer} failed: {e}")));
        }

        let read_result = read_framed(&mut stream).await;
        match read_result {
            Ok(resp) => {
                self.release(&endpoint, stream, permit).await;
                Ok(resp)
            }
            Err(e) => Err(ErrorBadGateway(format!("tcp read from {peer} failed: {e}"))),
        }
    }
}

impl Default for TcpPeerClient {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl PeerClient for TcpPeerClient {
    async fn put_shard(&self, peer: &str, bucket: &str, key: &str, shard_idx: usize, data: Vec<u8>) -> Result<(), Error> {
        let mut fbb = FlatBufferBuilder::new();
        let bucket_off = fbb.create_string(bucket);
        let key_off = fbb.create_string(key);
        let data_off = fbb.create_vector(&data);
        let req = PutShardRequest::create(
            &mut fbb,
            &PutShardRequestArgs {
                bucket: Some(bucket_off),
                key: Some(key_off),
                shard_idx: shard_idx as u32,
                data: Some(data_off),
            },
        );
        let envelope = Envelope::create(
            &mut fbb,
            &EnvelopeArgs {
                body_type: Body::PutShardRequest,
                body: Some(req.as_union_value()),
            },
        );
        fbb.finish(envelope, None);

        let resp_bytes = self.roundtrip(peer, fbb.finished_data()).await?;
        let resp_envelope = root_as_envelope(&resp_bytes)
            .map_err(|e| ErrorBadGateway(format!("malformed response from {peer}: {e}")))?;
        let resp = resp_envelope
            .body_as_put_shard_response()
            .ok_or_else(|| ErrorBadGateway(format!("unexpected response type from {peer}")))?;
        if !resp.ok() {
            return Err(ErrorBadGateway(format!(
                "put_shard to {peer} failed: {}",
                resp.error().unwrap_or("unknown error")
            )));
        }
        Ok(())
    }

    async fn get_shard(&self, peer: &str, bucket: &str, key: &str, shard_idx: usize) -> Result<Vec<u8>, Error> {
        let mut fbb = FlatBufferBuilder::new();
        let bucket_off = fbb.create_string(bucket);
        let key_off = fbb.create_string(key);
        let req = GetShardRequest::create(
            &mut fbb,
            &GetShardRequestArgs {
                bucket: Some(bucket_off),
                key: Some(key_off),
                shard_idx: shard_idx as u32,
            },
        );
        let envelope = Envelope::create(
            &mut fbb,
            &EnvelopeArgs {
                body_type: Body::GetShardRequest,
                body: Some(req.as_union_value()),
            },
        );
        fbb.finish(envelope, None);

        let resp_bytes = self.roundtrip(peer, fbb.finished_data()).await?;
        let resp_envelope = root_as_envelope(&resp_bytes)
            .map_err(|e| ErrorBadGateway(format!("malformed response from {peer}: {e}")))?;
        let resp = resp_envelope
            .body_as_get_shard_response()
            .ok_or_else(|| ErrorBadGateway(format!("unexpected response type from {peer}")))?;
        if !resp.ok() {
            return Err(ErrorBadGateway(format!(
                "get_shard from {peer} failed: {}",
                resp.error().unwrap_or("unknown error")
            )));
        }
        Ok(resp.data().map(|d| d.bytes().to_vec()).unwrap_or_default())
    }
}

// ---------------------------------------------------------------------------
// Server side: a plain TCP listener, one task per connection, looping over
// however many requests the client sends on that connection before it's
// returned to the client's pool (or dropped).
// ---------------------------------------------------------------------------

async fn handle_request(body: &[u8]) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::new();

    let envelope = match root_as_envelope(body) {
        Ok(e) => e,
        Err(_) => {
            // Malformed request: nothing sensible to reply with a type for;
            // the connection will fail on the client's framing check instead.
            let err_str = fbb.create_string("malformed request");
            let resp = PutShardResponse::create(
                &mut fbb,
                &PutShardResponseArgs { ok: false, error: Some(err_str) },
            );
            let out = Envelope::create(&mut fbb, &EnvelopeArgs { body_type: Body::PutShardResponse, body: Some(resp.as_union_value()) });
            fbb.finish(out, None);
            return fbb.finished_data().to_vec();
        }
    };

    match envelope.body_type() {
        Body::PutShardRequest => {
            let req = envelope.body_as_put_shard_request().expect("checked by body_type");
            let bucket = req.bucket().unwrap_or_default().to_string();
            let key = req.key().unwrap_or_default().to_string();
            let shard_idx = req.shard_idx() as usize;
            let data = req.data().map(|d| d.bytes().to_vec()).unwrap_or_default();

            let result = store_shard(&bucket, &key, shard_idx, &data);
            let (ok, error) = match &result {
                Ok(()) => (true, None),
                Err(e) => (false, Some(fbb.create_string(&e.to_string()))),
            };
            let resp = PutShardResponse::create(&mut fbb, &PutShardResponseArgs { ok, error });
            let out = Envelope::create(&mut fbb, &EnvelopeArgs { body_type: Body::PutShardResponse, body: Some(resp.as_union_value()) });
            fbb.finish(out, None);
        }
        Body::GetShardRequest => {
            let req = envelope.body_as_get_shard_request().expect("checked by body_type");
            let bucket = req.bucket().unwrap_or_default().to_string();
            let key = req.key().unwrap_or_default().to_string();
            let shard_idx = req.shard_idx() as usize;

            match load_shard(&bucket, &key, shard_idx) {
                Ok(data) => {
                    let data_off = fbb.create_vector(&data);
                    let resp = GetShardResponse::create(&mut fbb, &GetShardResponseArgs { ok: true, error: None, data: Some(data_off) });
                    let out = Envelope::create(&mut fbb, &EnvelopeArgs { body_type: Body::GetShardResponse, body: Some(resp.as_union_value()) });
                    fbb.finish(out, None);
                }
                Err(e) => {
                    let error = Some(fbb.create_string(&e.to_string()));
                    let resp = GetShardResponse::create(&mut fbb, &GetShardResponseArgs { ok: false, error, data: None });
                    let out = Envelope::create(&mut fbb, &EnvelopeArgs { body_type: Body::GetShardResponse, body: Some(resp.as_union_value()) });
                    fbb.finish(out, None);
                }
            }
        }
        _ => {
            let err_str = fbb.create_string("unknown request type");
            let resp = PutShardResponse::create(
                &mut fbb,
                &PutShardResponseArgs { ok: false, error: Some(err_str) },
            );
            let out = Envelope::create(&mut fbb, &EnvelopeArgs { body_type: Body::PutShardResponse, body: Some(resp.as_union_value()) });
            fbb.finish(out, None);
        }
    }

    fbb.finished_data().to_vec()
}

async fn handle_connection(mut stream: TcpStream) {
    loop {
        let body = match read_framed(&mut stream).await {
            Ok(b) => b,
            Err(_) => return, // client closed the connection (or a real error) — either way, done
        };
        let response = handle_request(&body).await;
        if write_framed(&mut stream, &response).await.is_err() {
            return;
        }
    }
}

pub async fn serve(addr: std::net::SocketAddr) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    loop {
        let (stream, _) = listener.accept().await?;
        let _ = stream.set_nodelay(true);
        tokio::spawn(handle_connection(stream));
    }
}

