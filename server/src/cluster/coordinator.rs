//! Cluster PUT/GET/DELETE handlers: resolve placement, fan out to peers
//! concurrently, and apply the MinIO-style quorum rule (reads succeed once
//! `k` shards respond, writes succeed once `k` peers acknowledge, `k+1` if
//! `m == k`). See docs/Distributed-Engine-Plan.md.
//!
//! PUT resolves `PlacementPolicy` fresh (over the coordinator's current
//! peer list) and pins the result into `LocationStore`. GET and DELETE
//! never call `PlacementPolicy` — they read the pin. This split is what
//! makes "existing objects are never moved when the peer list changes"
//! actually true (see the architecture doc for why recomputing on every
//! read would break that guarantee).

use actix_web::error::{ErrorBadRequest, ErrorForbidden, ErrorInternalServerError, ErrorNotFound};
use actix_web::{web, Error, HttpResponse};
use futures::future::join_all;
use log::{info, warn};
use serde::Deserialize;
use std::sync::Arc;

use super::ec::ErasureCoder;
use super::location_store::{LocationRecord, LocationStore};
use super::membership::Membership;
use super::peer_client::PeerClient;
use super::placement::PlacementPolicy;

pub struct ClusterState {
    pub membership: Arc<Membership>,
    pub placement: Arc<dyn PlacementPolicy>,
    pub ec: Arc<dyn ErasureCoder>,
    pub location_store: Arc<dyn LocationStore>,
    pub peer_client: Arc<dyn PeerClient>,
    /// Plain HTTP client for replicating `LocationRecord`s to peers. A
    /// separate concern from `PeerClient` (shard data transport, which is
    /// the thing the HTTP-vs-gRPC decision point swaps) — this is small,
    /// infrequent metadata, not worth a second trait boundary.
    pub location_http: reqwest::Client,
    pub timing: Arc<super::timing_stats::TimingStats>,
}

/// Write succeeds once this many of the `k+m` peers acknowledge: `k`, or
/// `k+1` in the edge case where parity count equals data-shard count —
/// MinIO's own quorum rule, verified against its docs (see architecture).
fn required_write_acks(k: usize, m: usize) -> usize {
    if m == k {
        k + 1
    } else {
        k
    }
}

/// Replicate a `LocationRecord` to every peer in `peers` (symmetric,
/// including this node, via the same internal HTTP call — no special
/// self-case). Returns how many acknowledged.
async fn replicate_location_put(state: &ClusterState, record: &LocationRecord, peers: &[String]) -> usize {
    let puts = peers.iter().cloned().map(|peer| {
        let client = state.location_http.clone();
        let record = record.clone();
        async move {
            let url = format!("{}/cluster/_internal/location", peer.trim_end_matches('/'));
            match client.post(&url).json(&record).send().await {
                Ok(r) if r.status().is_success() => true,
                Ok(r) => {
                    warn!("location pin replication to {peer} returned {}", r.status());
                    false
                }
                Err(e) => {
                    warn!("location pin replication to {peer} failed: {e}");
                    false
                }
            }
        }
    });
    join_all(puts).await.into_iter().filter(|ok| *ok).count()
}

/// Replicate a tombstone (delete) for `(bucket, key)` to every peer in
/// `peers`. Same symmetric-including-self treatment as the put path.
async fn replicate_location_delete(state: &ClusterState, bucket: &str, key: &str, peers: &[String]) -> usize {
    let deletes = peers.iter().cloned().map(|peer| {
        let client = state.location_http.clone();
        let bucket = bucket.to_string();
        let key = key.to_string();
        async move {
            let url = format!(
                "{}/cluster/_internal/location/{}/{}",
                peer.trim_end_matches('/'),
                bucket,
                key
            );
            match client.delete(&url).send().await {
                Ok(r) if r.status().is_success() => true,
                Ok(r) => {
                    warn!("location tombstone replication to {peer} returned {}", r.status());
                    false
                }
                Err(e) => {
                    warn!("location tombstone replication to {peer} failed: {e}");
                    false
                }
            }
        }
    });
    join_all(deletes).await.into_iter().filter(|ok| *ok).count()
}

pub async fn cluster_put_object(
    path: web::Path<(String, String)>,
    body: web::Bytes,
    state: web::Data<ClusterState>,
) -> Result<HttpResponse, Error> {
    let request_start = std::time::Instant::now();
    let (bucket, key) = path.into_inner();
    let peers = state.membership.peers();
    if peers.is_empty() {
        return Err(ErrorInternalServerError(
            "no peers configured (set WARPDRIVE_PEERS or join the cluster first)",
        ));
    }

    let k = state.ec.k();
    let m = state.ec.m();
    if peers.len() < k + m {
        return Err(ErrorInternalServerError(format!(
            "need at least {} peers for RS({k},{m}), have {}",
            k + m,
            peers.len()
        )));
    }

    // Placement resolved fresh, against the peer list as it is *right now*.
    // This is the only place ComputedPlacement is ever called for this key.
    let chosen = state.placement.place(&bucket, &key, &peers, k, m);

    let t0 = std::time::Instant::now();
    let encoded = state
        .ec
        .encode(&body)
        .map_err(|e| ErrorInternalServerError(e.to_string()))?;
    state.timing.encode.record(t0.elapsed());

    let required_acks = required_write_acks(k, m);

    let t0 = std::time::Instant::now();
    let puts = chosen
        .iter()
        .cloned()
        .zip(encoded.shards.into_iter())
        .enumerate()
        .map(|(idx, (peer, shard))| {
            let client = Arc::clone(&state.peer_client);
            let bucket = bucket.clone();
            let key = key.clone();
            async move {
                let res = client.put_shard(&peer, &bucket, &key, idx, shard).await;
                if let Err(ref e) = res {
                    warn!("put_shard idx={idx} peer={peer} failed: {e}");
                }
                res
            }
        });

    let results = join_all(puts).await;
    state.timing.shard_fanout.record(t0.elapsed());
    let acked = results.iter().filter(|r| r.is_ok()).count();
    if acked < required_acks {
        return Err(ErrorInternalServerError(format!(
            "write quorum not met: {acked}/{required_acks} peers acknowledged (RS({k},{m}))"
        )));
    }

    // Pin the resolved placement — this, not the hash function, is what
    // makes reads stable across later membership changes. Replicated to
    // every shard-holder peer (not just written locally): a GET can land
    // on any node, and only a node holding a copy of this pin can find the
    // object at all. All peers are treated symmetrically, including this
    // one, via the same internal HTTP call — no special self-case.
    let record = LocationRecord {
        bucket: bucket.clone(),
        key: key.clone(),
        shard_peers: chosen.clone(),
        k,
        m,
        original_len: encoded.original_len,
        retention_mode: None,
        retain_until: None,
        legal_hold: false,
    };

    let t0 = std::time::Instant::now();
    let loc_acked = replicate_location_put(&state, &record, &chosen).await;
    state.timing.location_replicate.record(t0.elapsed());
    if loc_acked < required_acks {
        return Err(ErrorInternalServerError(format!(
            "location pin quorum not met: {loc_acked}/{required_acks} peers stored the placement record \
             (shard data was written, but the object may not be findable from every node)"
        )));
    }

    state.timing.total.record(request_start.elapsed());
    info!("cluster PUT bucket={bucket} key={key} acked={acked}/{required_acks} loc_acked={loc_acked}/{required_acks}");
    Ok(HttpResponse::Ok().finish())
}

/// Diagnostic endpoint: phase-level latency breakdown for `cluster_put_object`,
/// so a load test can show where time actually goes (added specifically to
/// answer "where are we slow if we're not saturating anywhere" — see
/// docs/Distributed-Engine-Plan.md).
pub async fn cluster_timing_stats(state: web::Data<ClusterState>) -> HttpResponse {
    HttpResponse::Ok().content_type("text/plain").body(state.timing.summary())
}

/// Server-side breakdown of `store_shard` (the gRPC `ShardService::put_shard`
/// handler) — separate from `cluster_timing_stats`, which is the
/// coordinator's (client) view. See `shard_storage.rs`.
pub async fn cluster_shard_server_timing() -> HttpResponse {
    HttpResponse::Ok()
        .content_type("text/plain")
        .body(super::shard_storage::SERVER_TIMING.summary())
}

/// Client-side channel-lookup vs. RPC-call breakdown — see
/// `grpc_peer_client.rs::CLIENT_TIMING`.
pub async fn cluster_grpc_client_timing() -> HttpResponse {
    HttpResponse::Ok()
        .content_type("text/plain")
        .body(super::grpc_peer_client::CLIENT_TIMING.summary())
}

pub async fn cluster_get_object(
    path: web::Path<(String, String)>,
    state: web::Data<ClusterState>,
) -> Result<HttpResponse, Error> {
    let (bucket, key) = path.into_inner();

    // Never recompute placement here — look up the pin from write time.
    let record = state
        .location_store
        .get(&bucket, &key)
        .ok_or_else(|| ErrorNotFound("object not found"))?;

    let gets = record.shard_peers.iter().cloned().enumerate().map(|(idx, peer)| {
        let client = Arc::clone(&state.peer_client);
        let bucket = bucket.clone();
        let key = key.clone();
        async move {
            match client.get_shard(&peer, &bucket, &key, idx).await {
                Ok(data) => Some((idx, data)),
                Err(e) => {
                    warn!("get_shard idx={idx} peer={peer} failed: {e}");
                    None
                }
            }
        }
    });

    let results = join_all(gets).await;
    let mut shards: Vec<Option<Vec<u8>>> = vec![None; record.shard_peers.len()];
    for (idx, data) in results.into_iter().flatten() {
        shards[idx] = Some(data);
    }

    let present = shards.iter().filter(|s| s.is_some()).count();
    if present < record.k {
        return Err(ErrorInternalServerError(format!(
            "read quorum not met: {present}/{} shards available",
            record.k
        )));
    }

    let data = state
        .ec
        .decode(&shards, record.original_len)
        .map_err(|e| ErrorInternalServerError(e.to_string()))?;

    Ok(HttpResponse::Ok().body(data))
}

pub async fn cluster_delete_object(
    path: web::Path<(String, String)>,
    state: web::Data<ClusterState>,
) -> Result<HttpResponse, Error> {
    let (bucket, key) = path.into_inner();

    let record = match state.location_store.get(&bucket, &key) {
        Some(r) => r,
        None => return Ok(HttpResponse::NotFound().finish()),
    };

    // Object lock enforcement: one record, one lookup, no distributed lock
    // manager (see location_store.rs and the architecture doc).
    if let (Some(mode), Some(until)) = (&record.retention_mode, &record.retain_until) {
        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
        if until.as_str() > now.as_str() {
            return Err(ErrorForbidden(format!(
                "object is locked under {mode} retention until {until}"
            )));
        }
    }
    if record.legal_hold {
        return Err(ErrorForbidden("object has an active legal hold"));
    }

    // Shard bytes are left for later background reclamation, matching the
    // existing single-node Storage::delete semantics (queue, don't block
    // on immediate space reclamation) — only the pin removal is
    // authoritative for whether a GET can still find the object. The
    // tombstone is replicated the same way the pin itself was written, to
    // every peer that might hold a copy.
    let required_acks = required_write_acks(record.k, record.m);
    let acked = replicate_location_delete(&state, &bucket, &key, &record.shard_peers).await;
    if acked < required_acks {
        return Err(ErrorInternalServerError(format!(
            "tombstone quorum not met: {acked}/{required_acks} peers removed the placement record"
        )));
    }

    Ok(HttpResponse::Ok().finish())
}

#[derive(Debug, Deserialize)]
pub struct RetentionRequest {
    pub mode: String, // "GOVERNANCE" | "COMPLIANCE"
    pub retain_until: String, // RFC3339
    #[serde(default)]
    pub legal_hold: bool,
}

/// Internal-only endpoint: cluster object lock doesn't mirror the full S3
/// XML retention API (that's an explicit non-goal) — just enough to prove
/// the piggyback-on-location_store.rs design works end to end.
pub async fn cluster_put_retention(
    path: web::Path<(String, String)>,
    body: web::Json<RetentionRequest>,
    state: web::Data<ClusterState>,
) -> Result<HttpResponse, Error> {
    let (bucket, key) = path.into_inner();

    if body.mode != "GOVERNANCE" && body.mode != "COMPLIANCE" {
        return Err(ErrorBadRequest("mode must be GOVERNANCE or COMPLIANCE"));
    }

    let mut record = state
        .location_store
        .get(&bucket, &key)
        .ok_or_else(|| ErrorNotFound("object not found"))?;

    record.retention_mode = Some(body.mode.clone());
    record.retain_until = Some(body.retain_until.clone());
    record.legal_hold = body.legal_hold;

    let required_acks = required_write_acks(record.k, record.m);
    let acked = replicate_location_put(&state, &record, &record.shard_peers.clone()).await;
    if acked < required_acks {
        return Err(ErrorInternalServerError(format!(
            "retention update quorum not met: {acked}/{required_acks} peers updated the record"
        )));
    }

    Ok(HttpResponse::Ok().finish())
}

/// Receiving side of location-pin replication: just store whatever record
/// the coordinator that handled the PUT/retention-update sends. Any node
/// can receive this, matching the symmetric "any node can be coordinator"
/// design.
pub async fn cluster_internal_put_location(
    body: web::Json<LocationRecord>,
    state: web::Data<ClusterState>,
) -> Result<HttpResponse, Error> {
    state
        .location_store
        .put(body.into_inner())
        .map_err(|e| ErrorInternalServerError(e.to_string()))?;
    Ok(HttpResponse::Ok().finish())
}

/// Receiving side of tombstone replication.
pub async fn cluster_internal_delete_location(
    path: web::Path<(String, String)>,
    state: web::Data<ClusterState>,
) -> Result<HttpResponse, Error> {
    let (bucket, key) = path.into_inner();
    state
        .location_store
        .delete(&bucket, &key)
        .map_err(|e| ErrorInternalServerError(e.to_string()))?;
    Ok(HttpResponse::Ok().finish())
}

#[derive(Debug, Deserialize, serde::Serialize)]
pub struct JoinRequest {
    pub peer: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct JoinResponse {
    pub peers: Vec<String>,
}

/// SeaweedFS/MinIO-pool-style additive registration: a new node calls this
/// on any existing peer, is added to that peer's in-memory list, and gets
/// the full list back so it (and, via one broadcast round, every other
/// known peer) converges on the same set. No consensus, no rebalancing of
/// already-placed data.
pub async fn cluster_join(
    body: web::Json<JoinRequest>,
    state: web::Data<ClusterState>,
) -> Result<HttpResponse, Error> {
    let peers = state.membership.add_peer(&body.peer);
    Ok(HttpResponse::Ok().json(JoinResponse { peers }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_quorum_is_k_normally_and_k_plus_1_when_m_equals_k() {
        assert_eq!(required_write_acks(9, 6), 9);
        assert_eq!(required_write_acks(4, 4), 5);
        assert_eq!(required_write_acks(4, 2), 4);
    }
}
