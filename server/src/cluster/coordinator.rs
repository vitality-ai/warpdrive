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
use std::collections::HashMap;
use std::sync::Arc;

use super::bucket_config::{BucketConfigStore, BucketPlacementConfig};
use super::content_location_store::{ContentLocationStore, UnitMeta};
use super::ec::ErasureCoder;
use super::location_store::{LocationRecord, LocationStore};
use super::membership::Membership;
use super::packing::StripePacker;
use super::peer_client::PeerClient;
use super::placement::PlacementPolicy;
use super::pushdown::ColumnCodec;
use crate::s3::handlers::common::{parse_range_header, RangeResult};
use base64::Engine;

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
    /// Phase 3: content-dependent placement. Registry, not a single
    /// `Arc<dyn StripePacker>` — a bucket's config (below) names which
    /// entry to use, so a user-defined packer only needs to implement
    /// `StripePacker` and be registered here, same as `FacPacker`, to
    /// become a third pluggable placement policy. `content_location_store`
    /// pins multi-stripe objects, separate from `location_store` since the
    /// record shapes differ (one peer set vs. many). See `packed.rs`.
    pub packers: HashMap<String, Arc<dyn StripePacker>>,
    pub content_location_store: Arc<dyn ContentLocationStore>,
    /// Per-bucket placement policy choice + overhead-threshold fallback
    /// config (see `bucket_config.rs`). Missing bucket -> `default_bucket_config`.
    pub bucket_config_store: Arc<dyn BucketConfigStore>,
    /// Pushdown's column-decoding registry (see `pushdown.rs`), keyed by
    /// the codec name a unit was tagged with at PUT time.
    pub column_codecs: HashMap<String, Arc<dyn ColumnCodec>>,
}

/// Write succeeds once this many of the `k+m` peers acknowledge: `k`, or
/// `k+1` in the edge case where parity count equals data-shard count —
/// MinIO's own quorum rule, verified against its docs (see architecture).
pub(crate) fn required_write_acks(k: usize, m: usize) -> usize {
    if m == k {
        k + 1
    } else {
        k
    }
}

/// Parses `x-warpd-computable-units`, the poster's own header spec — a flat
/// JSON array where each entry is either `[offset, len]` (the plain
/// storage/placement path: not pushdown-capable, `codec: "opaque"`,
/// compressibility 1.0) or `[offset, len, uncompressed_len, codec]` (also
/// usable for pushdown — see `pushdown.rs`). No format awareness here: the
/// client decides what a unit is and, optionally, how it's encoded.
fn parse_computable_units_header(header_val: &actix_web::http::header::HeaderValue) -> Result<Vec<UnitMeta>, Error> {
    let s = header_val
        .to_str()
        .map_err(|_| ErrorBadRequest("x-warpd-computable-units header is not valid UTF-8"))?;
    let parsed: Vec<serde_json::Value> = serde_json::from_str(s)
        .map_err(|e| ErrorBadRequest(format!("x-warpd-computable-units header is not a JSON array: {e}")))?;
    if parsed.is_empty() {
        return Err(ErrorBadRequest("x-warpd-computable-units must list at least one unit"));
    }

    parsed
        .into_iter()
        .enumerate()
        .map(|(i, entry)| {
            let arr = entry
                .as_array()
                .ok_or_else(|| ErrorBadRequest("each computable unit must be a JSON array"))?;
            if arr.len() < 2 {
                return Err(ErrorBadRequest("each computable unit needs at least [offset, len]"));
            }
            let offset = arr[0].as_u64().ok_or_else(|| ErrorBadRequest("offset must be an integer"))?;
            let len = arr[1].as_u64().ok_or_else(|| ErrorBadRequest("len must be an integer"))?;
            let (uncompressed_len, codec) = if arr.len() >= 4 {
                let u = arr[2]
                    .as_u64()
                    .ok_or_else(|| ErrorBadRequest("uncompressed_len must be an integer"))?;
                let c = arr[3]
                    .as_str()
                    .ok_or_else(|| ErrorBadRequest("codec must be a string"))?
                    .to_string();
                (u, c)
            } else {
                (len, "opaque".to_string())
            };
            // Optional 5th element: base64-encoded opaque metadata, passed
            // straight through to packing::Unit::metadata — see that
            // field's doc. Absent for every caller that doesn't need it.
            let metadata = if arr.len() >= 5 {
                let b64 = arr[4].as_str().ok_or_else(|| ErrorBadRequest("metadata must be a base64 string"))?;
                base64::engine::general_purpose::STANDARD
                    .decode(b64)
                    .map_err(|e| ErrorBadRequest(format!("metadata is not valid base64: {e}")))?
            } else {
                Vec::new()
            };
            Ok(UnitMeta { unit_id: format!("u{i}"), offset, len, uncompressed_len, codec, metadata })
        })
        .collect()
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
pub(crate) async fn replicate_location_delete(state: &ClusterState, bucket: &str, key: &str, peers: &[String]) -> usize {
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

/// Content-location counterpart of `replicate_location_delete` — needed so
/// a plain PUT that overwrites a key previously packed can clear the stale
/// `ContentLocationStore` pin (see `put_object_plain`'s overwrite handling)
/// and so `cluster_delete_object` can tombstone a packed object, neither of
/// which was possible before: a packed object lived *only* in
/// `ContentLocationStore`, which DELETE never touched, and a plain overwrite
/// never cleared the packed pin GET still preferred.
pub(crate) async fn replicate_content_location_delete(state: &ClusterState, bucket: &str, key: &str, peers: &[String]) -> usize {
    let deletes = peers.iter().cloned().map(|peer| {
        let client = state.location_http.clone();
        let bucket = bucket.to_string();
        let key = key.to_string();
        async move {
            let url = format!(
                "{}/cluster/_internal/content_location/{}/{}",
                peer.trim_end_matches('/'),
                bucket,
                key
            );
            match client.delete(&url).send().await {
                Ok(r) if r.status().is_success() => true,
                Ok(r) => {
                    warn!("content-location tombstone replication to {peer} returned {}", r.status());
                    false
                }
                Err(e) => {
                    warn!("content-location tombstone replication to {peer} failed: {e}");
                    false
                }
            }
        }
    });
    join_all(deletes).await.into_iter().filter(|ok| *ok).count()
}

/// Attaches `x-warpd-placement` (`plain`|`packed`) and, when a packing
/// decision was actually computed — whether it was used or rejected as
/// over-budget — `x-warpd-pack-overhead-pct` to a PUT response. This is
/// what makes "the system reports the cost of that choice" a real,
/// client-visible fact rather than something only ever logged
/// server-side: before this, the overhead number existed (it's what the
/// threshold check above already compares against) but was never handed
/// back to the caller in any form.
fn attach_placement_headers(mut resp: HttpResponse, placement: &'static str, overhead_pct: Option<f64>) -> HttpResponse {
    resp.headers_mut().insert(
        actix_web::http::header::HeaderName::from_static("x-warpd-placement"),
        actix_web::http::header::HeaderValue::from_static(placement),
    );
    if let Some(pct) = overhead_pct {
        if let Ok(val) = actix_web::http::header::HeaderValue::from_str(&format!("{pct:.4}")) {
            resp.headers_mut()
                .insert(actix_web::http::header::HeaderName::from_static("x-warpd-pack-overhead-pct"), val);
        }
    }
    resp
}

pub async fn cluster_put_object(
    path: web::Path<(String, String)>,
    body: web::Bytes,
    req: actix_web::HttpRequest,
    state: web::Data<ClusterState>,
) -> Result<HttpResponse, Error> {
    let (bucket, key) = path.into_inner();

    // Content-dependent placement is a *bucket*-level decision
    // (bucket_config.rs), the same as every other bucket setting in this
    // project (versioning, ACL, retention) — not something a client opts
    // into per PUT by sending a header. An unconfigured bucket returns
    // here immediately: no header parsing, no packing attempt, the exact
    // same cost as before this feature existed (see the North star's
    // before/after performance check).
    let Some(config) = state.bucket_config_store.get(&bucket) else {
        return put_object_plain(&bucket, &key, &body, &state)
            .await
            .map(|r| attach_placement_headers(r, "plain", None));
    };

    // An empty body can never be packed meaningfully (every bin would be
    // zero-length, which `reed_solomon_erasure` rejects outright, 500ing
    // what should be a trivially-valid empty-object PUT). There's also
    // nothing to gain from packing zero bytes. Short-circuit to plain
    // before any packer runs, the same way the plain path's own encoder
    // already floors shard length at 1 byte for this exact case.
    if body.is_empty() {
        return put_object_plain(&bucket, &key, &body, &state)
            .await
            .map(|r| attach_placement_headers(r, "plain", None));
    }

    // For a configured bucket, the header's job narrows to two things:
    // supplying real unit boundaries when the client has them, and acting
    // as a per-object escape hatch — the literal value "false" forces
    // plain erasure coding for just this one object, overriding the
    // bucket's default. Anything else is parsed as the usual
    // [[offset,len,...],...] list.
    let header_val = req.headers().get("x-warpd-computable-units");
    let forced_plain = header_val
        .and_then(|h| h.to_str().ok())
        .map(|s| s.trim().eq_ignore_ascii_case("false"))
        .unwrap_or(false);
    if forced_plain {
        info!("bucket={bucket} key={key} x-warpd-computable-units: false — explicit opt-out of this bucket's custom placement");
        return put_object_plain(&bucket, &key, &body, &state)
            .await
            .map(|r| attach_placement_headers(r, "plain", None));
    }

    // No header at all on a configured bucket: still attempt content-
    // dependent placement, treating the whole object as a single unit.
    // This needs no special-casing to stay safe — a single unit packed
    // under RS(k>1,m) always costs strictly more than plain EC (the other
    // k-1 bins pad to the seed's size with nothing to fill them), so the
    // overhead-threshold check below rejects it and falls back to plain
    // on its own, the same as any other over-budget pack.
    let units_header = match header_val {
        Some(h) => parse_computable_units_header(h)?,
        None => vec![UnitMeta {
            unit_id: "u0".to_string(),
            offset: 0,
            len: body.len() as u64,
            uncompressed_len: body.len() as u64,
            codec: "opaque".to_string(),
            metadata: Vec::new(),
        }],
    };

    // *Which* packing algorithm runs, and whether its result is even worth
    // using, is itself part of the bucket's config. This mirrors Fusion's
    // own mechanism (ASPLOS'25 §4.2): "a system-level hyperparameter...
    // the maximum additional storage overhead [tolerated]... if the
    // algorithm cannot construct stripes within the specified storage
    // budget, it defaults to erasure coding the object into fixed-sized
    // blocks" — made per-bucket instead of a single global constant. A bad
    // or unknown `packer_name` degrades to the plain path with a warning
    // rather than failing the write: a misconfigured option should never
    // be why a PUT fails.
    if let Some(packer) = state.packers.get(&config.packer_name) {
        let k = state.ec.k();
        let units: Vec<super::packing::Unit> = units_header
            .iter()
            .map(|u| super::packing::Unit { unit_id: u.unit_id.clone(), size: u.len as usize, metadata: u.metadata.clone() })
            .collect();
        let stripes = packer.pack(k, &units);
        let overhead = super::packing::overhead_pct(k, state.ec.m(), &units, &stripes);

        if overhead <= config.overhead_threshold_pct {
            return super::packed::put_object_content_dependent(&bucket, &key, &body, &units_header, stripes, &state)
                .await
                .map(|r| attach_placement_headers(r, "packed", Some(overhead)));
        }
        info!(
            "bucket={bucket} key={key} content-dependent overhead {overhead:.3}% exceeds bucket threshold \
             {:.3}% (packer={:?}) — falling back to plain erasure coding for this object",
            config.overhead_threshold_pct, config.packer_name
        );
        return put_object_plain(&bucket, &key, &body, &state)
            .await
            .map(|r| attach_placement_headers(r, "plain", Some(overhead)));
    } else {
        warn!(
            "bucket={bucket} key={key} bucket config names unknown packer {:?} — \
             falling back to plain erasure coding for this object",
            config.packer_name
        );
    }

    put_object_plain(&bucket, &key, &body, &state)
        .await
        .map(|r| attach_placement_headers(r, "plain", None))
}

async fn put_object_plain(bucket: &str, key: &str, body: &[u8], state: &ClusterState) -> Result<HttpResponse, Error> {
    let bucket = bucket.to_string();
    let key = key.to_string();
    let request_start = std::time::Instant::now();
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
    // *every* known peer, not just the k+m shard-holders: a GET can land
    // on any node (any node can be coordinator), and only a node holding a
    // copy of this pin can find the object at all. Quorum-acked (not
    // all-acked) so one unreachable peer doesn't fail an otherwise-healthy
    // write; in the common case (all peers reachable) every node ends up
    // with the pin, making "any node can serve GET" actually true rather
    // than only true for the k+m peers that happened to hold shards.
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
    let loc_acked = replicate_location_put(&state, &record, &peers).await;
    state.timing.location_replicate.record(t0.elapsed());
    if loc_acked < required_acks {
        return Err(ErrorInternalServerError(format!(
            "location pin quorum not met: {loc_acked}/{required_acks} peers stored the placement record \
             (shard data was written, but the object may not be findable from every node)"
        )));
    }

    // This key may have previously been written content-dependent (packed)
    // on a bucket whose config later changed, or simply have had its
    // overhead cross the threshold this time where it didn't before. GET
    // checks `content_location_store` first, so if that stale record is
    // left in place a client could PUT successfully here and still read
    // back the *old* packed bytes on the next GET. Clear it, best-effort:
    // a plain PUT having already satisfied its own quorum is the operation
    // that should be considered to have succeeded either way.
    if state.content_location_store.get(&bucket, &key).is_some() {
        let _ = state.content_location_store.delete(&bucket, &key);
        replicate_content_location_delete(&state, &bucket, &key, &peers).await;
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
    req: actix_web::HttpRequest,
    state: web::Data<ClusterState>,
) -> Result<HttpResponse, Error> {
    let (bucket, key) = path.into_inner();

    // A content-dependent PUT pins into a separate store (different record
    // shape — many stripes, not one shard set). Check there first: cheap,
    // and a key is either content-dependent or not, decided once at PUT
    // time by header presence.
    if let Some(record) = state.content_location_store.get(&bucket, &key) {
        let total_len = record.original_len as u64;
        // Range-aware fetch only reaches the stripes whose units actually
        // overlap the requested bytes (see packed.rs) — this is the whole
        // reason a DuckDB-style selective Parquet read benefits from
        // content-dependent placement and a plain object doesn't.
        let range_result = parse_range_header(&req, total_len);
        return match range_result {
            RangeResult::Valid(start, end) => {
                let data = super::packed::get_object_content_dependent_range(record, start, end, &state).await?;
                Ok(HttpResponse::build(actix_web::http::StatusCode::PARTIAL_CONTENT)
                    .insert_header(("Accept-Ranges", "bytes"))
                    .insert_header(("Content-Range", format!("bytes {start}-{end}/{total_len}")))
                    .body(data))
            }
            RangeResult::Unsatisfiable => Err(
                actix_web::error::ErrorRangeNotSatisfiable("the requested range is not valid for this object"),
            ),
            RangeResult::None => {
                let resp = super::packed::get_object_content_dependent(record, &state).await?;
                let mut resp = resp;
                resp.headers_mut().insert(
                    actix_web::http::header::HeaderName::from_static("accept-ranges"),
                    actix_web::http::header::HeaderValue::from_static("bytes"),
                );
                Ok(resp)
            }
        };
    }

    // Never recompute placement here — look up the pin from write time.
    let record = state
        .location_store
        .get(&bucket, &key)
        .ok_or_else(|| ErrorNotFound("object not found"))?;
    let total_len = record.original_len as u64;

    // No sub-object structure to be selective about here: a Range request
    // still requires a full fan-out and full EC decode, then an in-memory
    // slice. This is the expected, unoptimized baseline a Range-GET demo
    // compares against — the cost difference is the point.
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

    // Decode with *this object's own* k/m, not the node's process-wide
    // `state.ec` — those can disagree (a node restarted with different
    // `WARPDRIVE_RS_K`/`WARPDRIVE_RS_M`, or a cluster mid-migration to new
    // parameters) and `state.ec` silently wins today, which is wrong:
    // `record.k`/`record.m` were already being used for the quorum count
    // above, but not for the decode that quorum count exists to gate.
    // `ReedSolomonCoder::new` just builds a small Galois-field table, cheap
    // enough to construct per request rather than needing a shared cache.
    let decoder = super::ec::ReedSolomonCoder::new(record.k, record.m)
        .map_err(|e| ErrorInternalServerError(e.to_string()))?;
    let data = decoder
        .decode(&shards, record.original_len)
        .map_err(|e| ErrorInternalServerError(e.to_string()))?;

    match parse_range_header(&req, total_len) {
        RangeResult::Valid(start, end) => {
            let slice = data
                .get(start as usize..=end as usize)
                .ok_or_else(|| actix_web::error::ErrorRangeNotSatisfiable("range out of bounds"))?
                .to_vec();
            Ok(HttpResponse::build(actix_web::http::StatusCode::PARTIAL_CONTENT)
                .insert_header(("Accept-Ranges", "bytes"))
                .insert_header(("Content-Range", format!("bytes {start}-{end}/{total_len}")))
                .body(slice))
        }
        RangeResult::Unsatisfiable => Err(
            actix_web::error::ErrorRangeNotSatisfiable("the requested range is not valid for this object"),
        ),
        RangeResult::None => {
            Ok(HttpResponse::Ok().insert_header(("Accept-Ranges", "bytes")).body(data))
        }
    }
}

/// Diagnostic: the stored `ContentDependentRecord` itself (stripe
/// capacities, bin layout, peer placement) — not the object's bytes. Lets
/// an external workload driver compute real storage-overhead/metadata-cost
/// metrics from what the live `StripePacker` actually decided, rather than
/// a separate calculation, which is the point of this being a *real*
/// measurement and not the simulator's cost model. See
/// docs/Distributed-Engine-Plan.md's phase 3.
pub async fn cluster_content_record(
    path: web::Path<(String, String)>,
    state: web::Data<ClusterState>,
) -> Result<HttpResponse, Error> {
    let (bucket, key) = path.into_inner();
    let record = state
        .content_location_store
        .get(&bucket, &key)
        .ok_or_else(|| ErrorNotFound("no content-dependent record for this key"))?;
    Ok(HttpResponse::Ok().json(record))
}

pub async fn cluster_delete_object(
    path: web::Path<(String, String)>,
    state: web::Data<ClusterState>,
) -> Result<HttpResponse, Error> {
    let (bucket, key) = path.into_inner();

    // A key is either plain (`LocationStore`) or content-dependent/packed
    // (`ContentLocationStore`) — decided once, at PUT time, by which path
    // handled it. DELETE used to only ever check `LocationStore`, so a
    // packed object (which never has a `LocationStore` entry) returned 404
    // and was never actually removed. Check both; a key can even have a
    // *stale* entry in the other store left over from an earlier overwrite
    // that changed which path it took (see `put_object_plain`'s and
    // `put_object_content_dependent`'s overwrite handling) — clear both
    // unconditionally rather than assuming only one is ever present.
    let plain_record = state.location_store.get(&bucket, &key);
    let packed_record = state.content_location_store.get(&bucket, &key);

    if plain_record.is_none() && packed_record.is_none() {
        return Ok(HttpResponse::NotFound().finish());
    }

    // Object lock enforcement: one record, one lookup, no distributed lock
    // manager (see location_store.rs and the architecture doc). Retention
    // only exists on the plain record today (`cluster_put_retention` only
    // ever writes `LocationStore`) — a packed object has no lock fields to
    // check.
    if let Some(record) = &plain_record {
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
    }

    // Shard bytes are left for later background reclamation, matching the
    // existing single-node Storage::delete semantics (queue, don't block
    // on immediate space reclamation) — only the pin removal is
    // authoritative for whether a GET can still find the object. Tombstones
    // are replicated to every known peer (see `put_object_plain`'s matching
    // comment on why that's "every peer," not just the ones that happen to
    // hold a shard).
    let peers = state.membership.peers();

    if let Some(record) = &plain_record {
        let required_acks = required_write_acks(record.k, record.m);
        let acked = replicate_location_delete(&state, &bucket, &key, &peers).await;
        if acked < required_acks {
            return Err(ErrorInternalServerError(format!(
                "tombstone quorum not met: {acked}/{required_acks} peers removed the placement record"
            )));
        }
    }
    if let Some(record) = &packed_record {
        let required_acks = required_write_acks(record.k, record.m);
        let acked = replicate_content_location_delete(&state, &bucket, &key, &peers).await;
        if acked < required_acks {
            return Err(ErrorInternalServerError(format!(
                "tombstone quorum not met: {acked}/{required_acks} peers removed the content-dependent placement record"
            )));
        }
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

#[derive(Debug, Deserialize)]
pub struct BucketConfigRequest {
    pub packer_name: String,
    pub overhead_threshold_pct: f64,
}

/// Admin endpoint: sets a bucket's placement-policy/overhead-threshold
/// config (`bucket_config.rs`) and replicates it synchronously to *every*
/// known peer, requiring all of them to ack — unlike shard/location writes,
/// where quorum is enough (an unreachable replica just means a future read
/// retries another one), every node must agree on a bucket's policy, or two
/// coordinators could silently choose different layouts for "the same"
/// bucket depending on which one happens to handle a given PUT.
pub async fn cluster_put_bucket_config(
    path: web::Path<String>,
    body: web::Json<BucketConfigRequest>,
    state: web::Data<ClusterState>,
) -> Result<HttpResponse, Error> {
    let bucket = path.into_inner();
    let peers = state.membership.peers();
    if peers.is_empty() {
        return Err(ErrorInternalServerError(
            "no peers configured (set WARPDRIVE_PEERS or join the cluster first)",
        ));
    }

    let config = BucketPlacementConfig {
        bucket: bucket.clone(),
        packer_name: body.packer_name.clone(),
        overhead_threshold_pct: body.overhead_threshold_pct,
    };

    let puts = peers.iter().cloned().map(|peer| {
        let client = state.location_http.clone();
        let config = config.clone();
        async move {
            let url = format!("{}/cluster/_internal/bucket_config", peer.trim_end_matches('/'));
            client.post(&url).json(&config).send().await.map(|r| r.status().is_success()).unwrap_or(false)
        }
    });
    let results = join_all(puts).await;
    let acked = results.iter().filter(|ok| **ok).count();
    if acked < peers.len() {
        return Err(ErrorInternalServerError(format!(
            "bucket config requires every peer to ack: {acked}/{} acknowledged",
            peers.len()
        )));
    }

    Ok(HttpResponse::Ok().finish())
}

/// Receiving side of bucket-config replication.
pub async fn cluster_internal_put_bucket_config(
    body: web::Json<BucketPlacementConfig>,
    state: web::Data<ClusterState>,
) -> Result<HttpResponse, Error> {
    state
        .bucket_config_store
        .put(body.into_inner())
        .map_err(|e| ErrorInternalServerError(e.to_string()))?;
    Ok(HttpResponse::Ok().finish())
}

/// Receiving side of content-dependent location-pin replication — the
/// multi-stripe counterpart to `cluster_internal_put_location` below.
pub async fn cluster_internal_put_content_location(
    body: web::Json<super::content_location_store::ContentDependentRecord>,
    state: web::Data<ClusterState>,
) -> Result<HttpResponse, Error> {
    state
        .content_location_store
        .put(body.into_inner())
        .map_err(|e| ErrorInternalServerError(e.to_string()))?;
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

/// Receiving side of content-location tombstone replication — the
/// multi-stripe counterpart to `cluster_internal_delete_location` above.
pub async fn cluster_internal_delete_content_location(
    path: web::Path<(String, String)>,
    state: web::Data<ClusterState>,
) -> Result<HttpResponse, Error> {
    let (bucket, key) = path.into_inner();
    state
        .content_location_store
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
