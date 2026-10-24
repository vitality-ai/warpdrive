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

use actix_web::error::{ErrorBadRequest, ErrorForbidden, ErrorInternalServerError, ErrorNotFound, ErrorPayloadTooLarge};
use actix_web::{web, Error, HttpResponse};
use futures::future::join_all;
use log::{info, warn};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

use super::bucket_config::{BucketConfigStore, BucketPlacementConfig};
use super::content_location_store::{ContentDependentRecord, ContentLocationStore, UnitMeta};
use super::ec::ErasureCoder;
use super::location_store::{LocationRecord, LocationStore, ObjectS3Metadata, UNAUTHENTICATED_USER};
use super::membership::Membership;
use super::packing::StripePacker;
use super::peer_client::PeerClient;
use super::placement::PlacementPolicy;
use super::pushdown::ColumnCodec;
use super::shard_storage::{new_version_id, versioned_key};
use crate::s3::handlers::checksum::ChecksumAlgorithm;
use crate::s3::handlers::common::{metadata_value_header, parse_range_header, RangeResult};
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
    // Phase 4 multipart upload bookkeeping deliberately has no field here:
    // both the part bytes and the upload "header" (status, content-type,
    // user-metadata) are ordinary objects through the existing
    // `location_store`/`put_object_plain` path, replicated to every peer
    // the same way as any other write -- see `multipart_meta_key`'s own
    // doc in coordinator.rs for why that, not a separate per-node store,
    // is what keeps "any node can coordinate any request" true for
    // multipart too.
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

/// Shared fan-out for every `replicate_*` function below: build one request
/// per peer (symmetric, including this node, via the same internal HTTP
/// call -- no special self-case), send them all concurrently, and count
/// how many succeeded. `build` constructs the actual `RequestBuilder`
/// (method + body) for a given peer's URL -- the one thing that genuinely
/// differs between a location/content-location put/delete -- everything
/// else (fan-out, success/failure counting, the warn! on a bad response or
/// a transport error) was previously duplicated four times, once per
/// record-shape/verb combination.
async fn replicate_to_peers<F>(state: &ClusterState, peers: &[String], path_suffix: &str, label: &str, build: F) -> usize
where
    F: Fn(&reqwest::Client, &str) -> reqwest::RequestBuilder,
{
    let requests = peers.iter().map(|peer| {
        let client = state.location_http.clone();
        let url = format!("{}/cluster/_internal/{path_suffix}", peer.trim_end_matches('/'));
        let req = build(&client, &url);
        async move {
            match req.send().await {
                Ok(r) if r.status().is_success() => true,
                Ok(r) => {
                    warn!("{label} replication to {peer} returned {}", r.status());
                    false
                }
                Err(e) => {
                    warn!("{label} replication to {peer} failed: {e}");
                    false
                }
            }
        }
    });
    join_all(requests).await.into_iter().filter(|ok| *ok).count()
}

/// Replicate a `LocationRecord` to every peer in `peers`. Returns how many
/// acknowledged.
async fn replicate_location_put(state: &ClusterState, record: &LocationRecord, peers: &[String]) -> usize {
    replicate_to_peers(state, peers, "location", "location pin", |client, url| client.post(url).json(record)).await
}

/// Content-dependent counterpart of `replicate_location_put` -- same
/// reasoning, same mechanism, just the other record shape. Used by
/// Phase 4's tagging support, which (unlike retention) applies to packed
/// objects too.
async fn replicate_content_location_put(state: &ClusterState, record: &ContentDependentRecord, peers: &[String]) -> usize {
    replicate_to_peers(state, peers, "content_location", "content-location pin", |client, url| client.post(url).json(record)).await
}

/// Replicate a tombstone (delete) for `(user_id, bucket, key)` to every
/// peer in `peers`.
pub(crate) async fn replicate_location_delete(state: &ClusterState, user_id: &str, bucket: &str, key: &str, peers: &[String]) -> usize {
    let body = DeleteKeyRequest { user_id: user_id.to_string(), bucket: bucket.to_string(), key: key.to_string() };
    replicate_to_peers(state, peers, "location", "location tombstone", |client, url| client.delete(url).json(&body)).await
}

/// Content-location counterpart of `replicate_location_delete` — needed so
/// a plain PUT that overwrites a key previously packed can clear the stale
/// `ContentLocationStore` pin (see `put_object_plain`'s overwrite handling)
/// and so `cluster_delete_object` can tombstone a packed object, neither of
/// which was possible before: a packed object lived *only* in
/// `ContentLocationStore`, which DELETE never touched, and a plain overwrite
/// never cleared the packed pin GET still preferred.
pub(crate) async fn replicate_content_location_delete(state: &ClusterState, user_id: &str, bucket: &str, key: &str, peers: &[String]) -> usize {
    let body = DeleteKeyRequest { user_id: user_id.to_string(), bucket: bucket.to_string(), key: key.to_string() };
    replicate_to_peers(state, peers, "content_location", "content-location tombstone", |client, url| client.delete(url).json(&body)).await
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

/// Attaches intra-object-pushdown read telemetry to a content-dependent GET
/// response: `X-Warpd-Stripes-Fetched`/`-Total` (how many of the object's
/// stripes this request actually touched) and `X-Warpd-Shard-Bytes-Read` (bytes
/// read off the shard backing store to serve it). A selective read of a
/// content-placed object touches only the stripes holding the requested units,
/// so these numbers are far below the whole-object cost -- which is the entire
/// point of content-dependent placement, now a client-visible, measurable fact.
fn attach_fetch_stat_headers(resp: &mut actix_web::HttpResponseBuilder, stat: &super::packed::FetchStat) {
    resp.insert_header(("x-warpd-stripes-fetched", stat.stripes_fetched.to_string()));
    resp.insert_header(("x-warpd-stripes-total", stat.stripes_total.to_string()));
    resp.insert_header(("x-warpd-shard-bytes-read", stat.shard_bytes.to_string()));
    SHARD_BYTES_READ.fetch_add(stat.shard_bytes, std::sync::atomic::Ordering::Relaxed);
}

/// Decodes an AWS SigV4 "streaming payload" body: repeated
/// `<hex-chunk-size>;chunk-signature=<sig>\r\n<chunk-bytes>\r\n`, ending in
/// a zero-size chunk. Chunk signatures are not re-verified here (same
/// no-auth-check posture as the rest of the cluster API) -- this only
/// strips the framing so the stored object is the real payload bytes,
/// not the wire encoding.
pub(crate) fn decode_aws_chunked_payload(raw: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(raw.len());
    let mut pos = 0usize;
    loop {
        let header_end = raw[pos..]
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or("malformed chunk: no CRLF after chunk header")?;
        let header = std::str::from_utf8(&raw[pos..pos + header_end])
            .map_err(|_| "malformed chunk header: not UTF-8".to_string())?;
        let size_str = header.split(';').next().unwrap_or("");
        let size = usize::from_str_radix(size_str.trim(), 16)
            .map_err(|_| format!("malformed chunk size: {size_str:?}"))?;
        pos += header_end + 2;

        if size == 0 {
            break;
        }
        if pos + size > raw.len() {
            return Err("malformed chunk: declared size exceeds remaining body".into());
        }
        out.extend_from_slice(&raw[pos..pos + size]);
        pos += size;
        if raw.get(pos..pos + 2) != Some(b"\r\n") {
            return Err("malformed chunk: missing trailing CRLF after chunk data".into());
        }
        pos += 2;
    }
    Ok(out)
}

/// Bare, pre-auth route: no identity to supply, so every write here lands
/// in the `UNAUTHENTICATED_USER` namespace and carries no S3 metadata.
/// Kept exactly as before this split -- see `cluster_put_object_inner`
/// for the real logic, now also reachable from an authenticated caller.
pub async fn cluster_put_object(
    path: web::Path<(String, String)>,
    body: web::Bytes,
    req: actix_web::HttpRequest,
    state: web::Data<ClusterState>,
) -> Result<HttpResponse, Error> {
    let (bucket, key) = path.into_inner();
    cluster_put_object_inner(
        UNAUTHENTICATED_USER, bucket, key, body, &req, &state, &ObjectS3Metadata::default(),
    )
    .await
}

/// `user_id`/`s3_meta` are the Phase 3 seam: a bare pre-auth caller passes
/// `UNAUTHENTICATED_USER`/`Default::default()` (see `cluster_put_object`
/// above), an authenticated `/cluster/s3/...` caller passes the real
/// identity and the S3 metadata it parsed from request headers.
pub(crate) async fn cluster_put_object_inner(
    user_id: &str,
    bucket: String,
    key: String,
    body: web::Bytes,
    req: &actix_web::HttpRequest,
    state: &web::Data<ClusterState>,
    s3_meta: &ObjectS3Metadata,
) -> Result<HttpResponse, Error> {
    // Real S3 SDKs (minio-go, used by minio/warp and many other tools)
    // sign PUT bodies with AWS's "streaming payload" scheme when
    // content-length is known up front: `x-amz-content-sha256:
    // STREAMING-AWS4-HMAC-SHA256-PAYLOAD`, body wire-encoded as repeated
    // `<hex-chunk-size>;chunk-signature=<sig>\r\n<chunk-bytes>\r\n`,
    // terminated by a zero-size chunk. `body: web::Bytes` is the raw wire
    // bytes -- actix/hyper only unwraps HTTP-level Transfer-Encoding, not
    // this application-level AWS framing, so without decoding it here the
    // stored object silently includes chunk-size/signature bytes as part
    // of its content (confirmed via `x-amz-decoded-content-length`
    // mismatching the raw body length). The legacy single-node S3 path
    // (s3/handlers/object.rs) has the same gap -- it only strips
    // "aws-chunked" from the stored Content-Encoding metadata, never
    // decodes the body -- so this was latent there too, just never
    // exercised by a real streaming-signing SDK client until now.
    let body: web::Bytes = if req
        .headers()
        .get("x-amz-content-sha256")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("STREAMING-"))
    {
        web::Bytes::from(decode_aws_chunked_payload(&body).map_err(ErrorBadRequest)?)
    } else {
        body
    };

    // #150: `packed.rs::stripe_key` reserves a leading NUL byte to keep a
    // content-dependent object's per-stripe shard keys structurally
    // disjoint from any plain object's key, on the assumption that a real
    // key never starts with one. Enforced here, once, at the only path
    // that can ever create a new mapping for a key -- rather than leaving
    // it as an unenforced assumption -- so that assumption actually holds.
    if key.contains('\0') {
        return Err(ErrorBadRequest("object key must not contain a NUL byte"));
    }

    // Content-dependent placement is a *bucket*-level decision
    // (bucket_config.rs), the same as every other bucket setting in this
    // project (versioning, ACL, retention) — not something a client opts
    // into per PUT by sending a header. An unconfigured bucket returns
    // here immediately: no header parsing, no packing attempt, the exact
    // same cost as before this feature existed (see the North star's
    // before/after performance check).
    let Some(config) = state.bucket_config_store.get(&bucket) else {
        return put_object_plain(user_id, &bucket, &key, &body, state, s3_meta)
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
        return put_object_plain(user_id, &bucket, &key, &body, state, s3_meta)
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
        return put_object_plain(user_id, &bucket, &key, &body, state, s3_meta)
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

        // Two independent gates, both must pass to pack:
        //  1. storage: extra bytes vs plain EC must stay under the bucket budget
        //     (the Fusion overhead-threshold mechanism).
        //  2. read cost: the modeled per-query read cost of this stripe layout
        //     must beat plain at the expected selectivity -- a many-stripe layout
        //     (or a broad workload) pays a per-stripe fan-out that plain's single
        //     fetch group avoids, so pushing down can be a net loss. Disable
        //     pushdown smartly rather than ship a layout that reads slower.
        let (sel, rtt) = pushdown_cost_params();
        let (read_worth_it, packed_cost, plain_cost) =
            super::packing::pushdown_read_cost(body.len(), stripes.len(), sel, rtt);

        if overhead <= config.overhead_threshold_pct && read_worth_it {
            return super::packed::put_object_content_dependent(user_id, &bucket, &key, &body, &units_header, stripes, state, s3_meta)
                .await
                .map(|r| attach_placement_headers(r, "packed", Some(overhead)));
        }
        if overhead > config.overhead_threshold_pct {
            info!(
                "bucket={bucket} key={key} content-dependent overhead {overhead:.3}% exceeds bucket threshold \
                 {:.3}% (packer={:?}) — falling back to plain erasure coding for this object",
                config.overhead_threshold_pct, config.packer_name
            );
        } else {
            info!(
                "bucket={bucket} key={key} content-dependent read cost {packed_cost:.0} >= plain {plain_cost:.0} \
                 byte-equivalents at selectivity {sel:.2} over {} stripe(s) (packer={:?}) — fan-out would outweigh \
                 the pushdown benefit, falling back to plain",
                stripes.len(), config.packer_name
            );
        }
        return put_object_plain(user_id, &bucket, &key, &body, state, s3_meta)
            .await
            .map(|r| attach_placement_headers(r, "plain", Some(overhead)));
    } else {
        warn!(
            "bucket={bucket} key={key} bucket config names unknown packer {:?} — \
             falling back to plain erasure coding for this object",
            config.packer_name
        );
    }

    put_object_plain(user_id, &bucket, &key, &body, state, s3_meta)
        .await
        .map(|r| attach_placement_headers(r, "plain", None))
}

async fn put_object_plain(
    user_id: &str, bucket: &str, key: &str, body: &[u8], state: &ClusterState, s3_meta: &ObjectS3Metadata,
) -> Result<HttpResponse, Error> {
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

    // #159: a shard this transport can't carry used to fail silently deep
    // inside `PeerClient::put_shard` on every peer, surfacing only as
    // "write quorum not met: 0/N" with no hint why every single peer
    // refused. Checked upfront, against the active transport's own stated
    // limit, so an oversized object gets one clear error instead.
    let shard_len = body.len().div_ceil(k).max(1);
    if let Some(max) = state.peer_client.max_shard_size() {
        if shard_len > max {
            return Err(ErrorPayloadTooLarge(format!(
                "object too large for RS({k},{m}): each shard would be {shard_len} bytes, \
                 over this transport's {max}-byte limit (raise WARPDRIVE_GRPC_MAX_MESSAGE_SIZE, \
                 pick a larger k, or switch WARPDRIVE_PEER_TRANSPORT to http/tcp)"
            )));
        }
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

    // #151: every version of an object writes its shards under a brand
    // new key (`versioned_key`), never overwriting the previous version's
    // shards in place. This is what actually prevents corruption on a
    // failed or concurrent overwrite -- the pin below is only flipped to
    // this `version` after quorum, so until (and unless) that happens, a
    // reader still using the *old* pin keeps reading the old version's
    // shards, completely untouched by this write, however it turns out.
    let version = new_version_id();
    let wire_key = versioned_key(&key, &version);

    let t0 = std::time::Instant::now();
    let puts = chosen
        .iter()
        .cloned()
        .zip(encoded.shards.into_iter())
        .enumerate()
        .map(|(idx, (peer, shard))| {
            let client = Arc::clone(&state.peer_client);
            let bucket = bucket.clone();
            let wire_key = wire_key.clone();
            async move {
                let res = client.put_shard(&peer, user_id, &bucket, &wire_key, idx, shard).await;
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
        user_id: user_id.to_string(),
        bucket: bucket.clone(),
        key: key.clone(),
        shard_peers: chosen.clone(),
        k,
        m,
        original_len: encoded.original_len,
        version,
        retention_mode: None,
        retain_until: None,
        legal_hold: false,
        etag: s3_meta.etag.clone(),
        content_type: s3_meta.content_type.clone(),
        last_modified: s3_meta.last_modified.clone(),
        user_metadata: s3_meta.user_metadata.clone(),
        cache_control: s3_meta.cache_control.clone(),
        expires: s3_meta.expires.clone(),
        content_encoding: s3_meta.content_encoding.clone(),
        checksum_algorithm: s3_meta.checksum_algorithm.clone(),
        checksum_value: s3_meta.checksum_value.clone(),
        checksum_type: s3_meta.checksum_type.clone(),
        // A PUT (full overwrite) starts a conceptually new object version
        // with no tags, same as real S3 -- tags are (re-)applied via a
        // separate PutObjectTagging call, never inherited from whatever
        // the previous version had.
        tags: Vec::new(),
    };

    // This key may have previously been written content-dependent (packed)
    // on a bucket whose config later changed, or simply have had its
    // overhead cross the threshold this time where it didn't before. GET
    // checks `content_location_store` first, so if that stale record is
    // left in place a client could PUT successfully here and still read
    // back the *old* packed bytes on the next GET.
    //
    // Broadcast this tombstone unconditionally, not gated on a local
    // `.is_some()` check: this *coordinator* only needed quorum to receive
    // the original packed pin, not all of it, so it may have no local copy
    // even though other peers still do. Skipping the broadcast on a local
    // miss would leave those other peers serving stale packed bytes
    // forever. Delete is idempotent on a receiver that has nothing to
    // delete, so there's no cost to always sending it (see issue #153 and
    // docs/reviews/v1.0.0-beta-code-review.md). Quorum-checked the same as
    // every other write here: a PUT that can't confirm the stale pin was
    // actually cleared cluster-wide is treated as not fully succeeded.
    //
    // Run concurrently with the location-pin replication below rather than
    // after it: the two broadcasts touch different stores/keys (the new
    // object's placement pin vs. a possibly-nonexistent old packed pin) and
    // neither's result depends on the other's, so there is no ordering
    // requirement between them -- only that both finish, and both quorums
    // are still checked independently afterward. This halves the number of
    // sequential cross-peer round trips a PUT pays for (was: shard fan-out,
    // then location-pin quorum, then cleanup-broadcast quorum, each waiting
    // on the last; now the latter two overlap).
    {
        // web::block: this store's `delete` now does a real group-commit
        // fsync (see `storage::group_commit`'s module doc) -- calling it
        // directly here would block this actix worker's whole event loop
        // for the fsync's duration, same hazard `cluster_internal_delete_*`
        // were fixed for. Best-effort local cleanup (the actual durability
        // guarantee is the quorum-checked broadcast below), so a failure
        // here doesn't fail the PUT, but it's logged rather than silently
        // discarded -- a real disk error (e.g. out of space) on this node
        // shouldn't vanish just because the write still succeeded cluster-wide.
        let store = Arc::clone(&state.content_location_store);
        let user_id_owned = user_id.to_string();
        let bucket_owned = bucket.clone();
        let key_owned = key.clone();
        let result = web::block(move || store.delete(&user_id_owned, &bucket_owned, &key_owned).map_err(|e| e.to_string())).await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => log::warn!("stale content-location pin cleanup failed for {bucket}/{key}: {e}"),
            Err(e) => log::warn!("stale content-location pin cleanup blocking task failed for {bucket}/{key}: {e}"),
        }
    }
    let t0 = std::time::Instant::now();
    let (loc_acked, cleanup_acked) = tokio::join!(
        replicate_location_put(&state, &record, &peers),
        replicate_content_location_delete(&state, user_id, &bucket, &key, &peers),
    );
    state.timing.location_replicate.record(t0.elapsed());
    if loc_acked < required_acks {
        return Err(ErrorInternalServerError(format!(
            "location pin quorum not met: {loc_acked}/{required_acks} peers stored the placement record \
             (shard data was written, but the object may not be findable from every node)"
        )));
    }
    if cleanup_acked < required_acks {
        return Err(ErrorInternalServerError(format!(
            "wrote the new object, but could not confirm the stale packed pin was cleared cluster-wide: \
             {cleanup_acked}/{required_acks} peers acknowledged the tombstone (a GET on an unacknowledged \
             peer may still return the old packed bytes)"
        )));
    }

    state.timing.total.record(request_start.elapsed());
    info!("cluster PUT bucket={bucket} key={key} acked={acked}/{required_acks} loc_acked={loc_acked}/{required_acks}");
    Ok(HttpResponse::Ok().finish())
}

/// Fetches and EC-decodes a plain object's full bytes -- the same fetch
/// loop `cluster_get_object_inner`'s no-range path uses, extracted so
/// multipart completion (below) can read a part's bytes back without
/// going through response-building. Only used for parts stored via
/// `put_object_plain` (see `cluster_upload_part_inner`), so there's no
/// content-dependent (packed) case to handle here.
async fn fetch_plain_object_bytes(user_id: &str, bucket: &str, key: &str, state: &web::Data<ClusterState>) -> Result<Vec<u8>, Error> {
    let record = state
        .location_store
        .get(user_id, bucket, key)
        .ok_or_else(|| ErrorNotFound("object not found"))?;

    let wire_key = if record.version.is_empty() {
        key.to_string()
    } else {
        versioned_key(key, &record.version)
    };

    let gets = record.shard_peers.iter().cloned().enumerate().map(|(idx, peer)| {
        let client = Arc::clone(&state.peer_client);
        let bucket = bucket.to_string();
        let wire_key = wire_key.clone();
        async move { client.get_shard(&peer, user_id, &bucket, &wire_key, idx).await.ok().map(|d| (idx, d)) }
    });
    let results = join_all(gets).await;
    let mut shards: Vec<Option<Vec<u8>>> = vec![None; record.shard_peers.len()];
    for (idx, data) in results.into_iter().flatten() {
        shards[idx] = Some(data);
    }
    let present = shards.iter().filter(|s| s.is_some()).count();
    if present < record.k {
        return Err(ErrorInternalServerError(format!("read quorum not met: {present}/{} shards available", record.k)));
    }
    let decoder = super::ec::ReedSolomonCoder::new(record.k, record.m).map_err(|e| ErrorInternalServerError(e.to_string()))?;
    decoder.decode(&shards, record.original_len).map_err(|e| ErrorInternalServerError(e.to_string()))
}

/// Synthetic key for one multipart part's bytes, stored via the ordinary
/// plain EC path. Same NUL-byte key-space-disjointness scheme as
/// `packed.rs::stripe_key` (a real object key is rejected if it starts
/// with `\0`, enforced in `cluster_put_object_inner`), with its own
/// distinct literal prefix (`mpu:` vs. `cdstripe`, and vs. `mpu-meta:`
/// below) so none of these synthetic namespaces can ever collide.
fn multipart_part_key(upload_id: &str, part_number: i32) -> String {
    format!("\0mpu:{}:{upload_id}:{part_number}", upload_id.len())
}

/// Prefix matching every part key for one upload -- `upload_id` is
/// guaranteed `:`-free (`new_version_id`'s own doc), so splitting a
/// listed key on its *last* `:` to recover `part_number` is unambiguous,
/// the same guarantee `shard_key`/`versioned_key` already rely on.
fn multipart_part_prefix(upload_id: &str) -> String {
    format!("\0mpu:{}:{upload_id}:", upload_id.len())
}

/// Synthetic key for an upload's own bookkeeping "header" -- deliberately
/// an ordinary plain object through the exact same replicated write path
/// as everything else in this file, not a separate per-node store. Its
/// *existence* in `location_store` (replicated to every peer, like any
/// other PUT) means "upload in progress"; its absence means "completed,
/// aborted, or never existed" -- no separate status field needed. Its
/// `content_type`/`user_metadata` fields (via `ObjectS3Metadata`, same as
/// any other object) carry what `CompleteMultipartUpload` needs to set on
/// the final object. This is what makes multipart bookkeeping visible
/// from *any* coordinating node, not just the one that happened to
/// receive `CreateMultipartUpload` -- the same "any node can coordinate
/// any request" symmetry every other cluster write already has.
fn multipart_meta_key(upload_id: &str) -> String {
    format!("\0mpu-meta:{upload_id}")
}

// ---------------------------------------------------------------------------
// Phase 4: multipart upload, authenticated-surface support functions.
// Scope deliberately narrower than single-node's multipart: only plain
// (non-content-dependent) final objects are supported. A content-
// dependent final object would need re-running the packer over the
// reassembled bytes, which is real, separate scope -- not attempted
// here, and not silently dropped either: `cluster_complete_multipart_
// upload_inner` always writes a plain final object, regardless of the
// destination bucket's packing config, and says so in its own doc.
// ---------------------------------------------------------------------------

pub(crate) async fn cluster_create_multipart_upload_inner(
    user_id: &str, bucket: &str, _key: &str, content_type: Option<String>, user_metadata: HashMap<String, String>,
    checksum_algorithm: Option<String>,
    state: &web::Data<ClusterState>,
) -> Result<String, Error> {
    let upload_id = super::shard_storage::new_version_id();
    // Remember which checksum algorithm the client asked for at initiate
    // (x-amz-sdk-checksum-algorithm / x-amz-checksum-algorithm / x-amz-trailer).
    // The header record carries it on its LocationRecord so CompleteMultipartUpload
    // can compute that same algorithm over the reassembled object and a later
    // checksum-mode GET returns the algorithm the client actually requested,
    // rather than a hardcoded default it would then reject as a mismatch.
    let s3_meta = ObjectS3Metadata { content_type, user_metadata, checksum_algorithm, ..Default::default() };
    // Empty body: the header carries no bytes of its own, only metadata
    // on the (fully replicated) LocationRecord it creates.
    put_object_plain(user_id, bucket, &multipart_meta_key(&upload_id), &[], state, &s3_meta).await?;
    Ok(upload_id)
}

/// Stores one part's bytes via the ordinary plain EC path under its
/// synthetic key. The part's own `LocationRecord` (etag, size) *is* the
/// bookkeeping -- replicated the same way as the part's bytes, no
/// separate index. Returns the part's ETag.
pub(crate) async fn cluster_upload_part_inner(
    user_id: &str, bucket: &str, upload_id: &str, part_number: i32, body: web::Bytes,
    checksum_algorithm: Option<String>,
    state: &web::Data<ClusterState>,
) -> Result<String, Error> {
    if state.location_store.get(user_id, bucket, &multipart_meta_key(upload_id)).is_none() {
        return Err(ErrorNotFound("no such upload"));
    }

    let etag = crate::s3::handlers::common::md5_etag(&body);
    // Record the checksum algorithm the client declared on this part (e.g.
    // `x-amz-trailer: x-amz-checksum-crc64nvme`). Clients like pyarrow declare
    // it per part rather than at CreateMultipartUpload, so CompleteMultipartUpload
    // recovers it from the stored part records to checksum the whole object.
    let s3_meta = ObjectS3Metadata { etag: Some(etag.clone()), checksum_algorithm, ..Default::default() };
    put_object_plain(user_id, bucket, &multipart_part_key(upload_id, part_number), &body, state, &s3_meta).await?;
    Ok(etag)
}

/// Lists an upload's parts via `LocationStore::list`'s prefix scan --
/// already replicated, already available on any node -- then one `get`
/// per listed key to recover its real etag (`list` itself only returns
/// key+size). Sorted by part number.
pub(crate) fn cluster_list_parts_inner(user_id: &str, bucket: &str, upload_id: &str, state: &web::Data<ClusterState>) -> Result<Vec<(i32, String, u64)>, Error> {
    if state.location_store.get(user_id, bucket, &multipart_meta_key(upload_id)).is_none() {
        return Err(ErrorNotFound("no such upload"));
    }
    let prefix = multipart_part_prefix(upload_id);
    let mut parts: Vec<(i32, String, u64)> = state
        .location_store
        .list(user_id, bucket, &prefix)
        .into_iter()
        .filter_map(|(part_key, size)| {
            let part_number: i32 = part_key.rsplit(':').next()?.parse().ok()?;
            let record = state.location_store.get(user_id, bucket, &part_key)?;
            Some((part_number, record.etag.unwrap_or_default(), size))
        })
        .collect();
    parts.sort_by_key(|(n, _, _)| *n);
    Ok(parts)
}

/// Reassembles the requested parts in order, writes the result as one
/// ordinary plain object under `final_key` (see this section's own doc
/// on why this is always plain, never re-packed), cleans up the header
/// and part records, and returns `(final_etag, total_size)`. `requested`
/// is `(part_number, client-sent etag)` pairs in the order the client
/// wants them assembled -- each is validated against the stored part's
/// own etag before use, same check single-node's `CompleteMultipartUpload`
/// does.
pub(crate) async fn cluster_complete_multipart_upload_inner(
    user_id: &str, bucket: &str, final_key: &str, upload_id: &str, requested: &[(i32, String)], state: &web::Data<ClusterState>,
) -> Result<(String, u64), Error> {
    let meta_record = state
        .location_store
        .get(user_id, bucket, &multipart_meta_key(upload_id))
        .ok_or_else(|| ErrorNotFound("no such upload"))?;

    // Validate every part up front (cheap local index lookups, no network),
    // bailing out on the first missing/mismatched part exactly like the
    // previous sequential version did. Only the actual byte fetch -- the
    // part that fans out to EC shards across peers -- is run concurrently
    // below: each `fetch_plain_object_bytes` call already parallelizes its
    // own shard fan-out, but a 50-part upload still paid 50 sequential
    // round-trip-groups before this, one per part, instead of one.
    let mut validated: Vec<(String, String)> = Vec::with_capacity(requested.len());
    // Recover the checksum algorithm the client declared per part (pyarrow and
    // similar clients put it on the UploadPart trailer, not at initiate). The
    // parts all share one algorithm, so the first one that carries it wins.
    let mut part_checksum_algorithm: Option<String> = None;
    for (part_number, client_etag) in requested {
        let part_key = multipart_part_key(upload_id, *part_number);
        let stored = state
            .location_store
            .get(user_id, bucket, &part_key)
            .ok_or_else(|| ErrorBadRequest("one or more of the specified parts could not be found"))?;
        if part_checksum_algorithm.is_none() {
            part_checksum_algorithm = stored.checksum_algorithm.clone();
        }
        let stored_etag = stored.etag.unwrap_or_default();
        if crate::s3::handlers::common::normalize_etag(client_etag) != crate::s3::handlers::common::normalize_etag(&stored_etag) {
            return Err(ErrorBadRequest("one or more of the specified parts could not be found"));
        }
        validated.push((part_key, stored_etag));
    }

    // `join_all` preserves input order in its output, so `part_bytes[i]`
    // still lines up with `validated[i]` -- required here, since the
    // reassembled body must be the parts concatenated in request order,
    // not whichever fetch happens to finish first.
    let fetches = validated.iter().map(|(part_key, _)| fetch_plain_object_bytes(user_id, bucket, part_key, state));
    let part_bytes: Vec<Vec<u8>> = join_all(fetches).await.into_iter().collect::<Result<_, _>>()?;

    let mut combined_md5_bytes: Vec<u8> = Vec::new();
    let mut body: Vec<u8> = Vec::new();
    for ((_, stored_etag), bytes) in validated.iter().zip(part_bytes) {
        if let Ok(raw) = hex::decode(crate::s3::handlers::common::normalize_etag(stored_etag)) {
            combined_md5_bytes.extend_from_slice(&raw);
        }
        body.extend_from_slice(&bytes);
    }

    // Same composite-ETag formula single-node's CompleteMultipartUpload
    // uses: md5 of the concatenated per-part md5 bytes, dash, part count.
    let final_etag = format!("\"{}-{}\"", hex::encode(md5::compute(&combined_md5_bytes).0), requested.len());
    let total_size = body.len() as u64;

    // Checksum on by default here too: a real checksum over the
    // *reassembled* bytes, so a later GET can verify the complete object,
    // not just rely on each part having been individually correct at
    // upload time. We honor the algorithm the client declared at initiate
    // (stored on the header record) so a strict client that asked for
    // CRC64NVME gets CRC64NVME back on a checksum-mode GET, not a default
    // it would reject. Absent a declared algorithm we fall back to CRC32C.
    let algo = part_checksum_algorithm.as_deref()
        .or(meta_record.checksum_algorithm.as_deref())
        .and_then(ChecksumAlgorithm::from_str)
        .unwrap_or(ChecksumAlgorithm::Crc32c);
    let s3_meta = ObjectS3Metadata {
        etag: Some(final_etag.clone()),
        content_type: meta_record.content_type.clone(),
        last_modified: Some(super::s3_surface::now_http_date()),
        user_metadata: meta_record.user_metadata.clone(),
        checksum_algorithm: Some(algo.as_str().to_string()),
        checksum_value: Some(crate::s3::handlers::checksum::compute_checksum(&algo, &body)),
        checksum_type: Some(algo.default_type().to_string()),
        ..Default::default()
    };
    put_object_plain(user_id, bucket, final_key, &body, state, &s3_meta).await?;

    // Local-only cleanup of the now-dead header/part records (no tombstone
    // replication to peers) -- a storage-reclamation gap, not a
    // correctness one: this project has no compaction/GC anywhere yet
    // (see RFC3), so leftover shard bytes on peers already accumulate
    // the same way for ordinary deletes. Tracked as the same class of
    // future work, not a new gap introduced here.
    //
    // web::block: `delete` now does a real group-commit fsync per call
    // (see `storage::group_commit`'s module doc), so this whole loop (one
    // blocking task, not one per part) must run off the actix worker's
    // event loop the same way the single-object delete paths were fixed.
    {
        let store = Arc::clone(&state.location_store);
        let user_id_owned = user_id.to_string();
        let bucket_owned = bucket.to_string();
        let upload_id_owned = upload_id.to_string();
        let part_numbers: Vec<i32> = requested.iter().map(|(n, _)| *n).collect();
        let result = web::block(move || {
            store.delete(&user_id_owned, &bucket_owned, &multipart_meta_key(&upload_id_owned))?;
            for part_number in part_numbers {
                store.delete(&user_id_owned, &bucket_owned, &multipart_part_key(&upload_id_owned, part_number))?;
            }
            Ok::<_, std::io::Error>(())
        })
        .await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => log::warn!("multipart cleanup failed for {bucket}/{upload_id}: {e}"),
            Err(e) => log::warn!("multipart cleanup blocking task failed for {bucket}/{upload_id}: {e}"),
        }
    }

    Ok((final_etag, total_size))
}

/// Aborts an in-progress upload: cleans up the header and part records
/// without writing a final object.
pub(crate) async fn cluster_abort_multipart_upload_inner(user_id: &str, bucket: &str, upload_id: &str, state: &web::Data<ClusterState>) -> Result<(), Error> {
    if state.location_store.get(user_id, bucket, &multipart_meta_key(upload_id)).is_none() {
        return Err(ErrorNotFound("no such upload"));
    }
    let prefix = multipart_part_prefix(upload_id);
    let part_keys: Vec<String> = state.location_store.list(user_id, bucket, &prefix).into_iter().map(|(k, _)| k).collect();

    // Best-effort local cleanup, same "storage-reclamation gap, not a
    // correctness one" reasoning as the matching cleanup in
    // `cluster_complete_multipart_upload_inner` -- a failure here doesn't
    // fail the abort itself. web::block (one blocking task, not one per
    // part): `delete` now does a real group-commit fsync per call.
    let store = Arc::clone(&state.location_store);
    let user_id_owned = user_id.to_string();
    let bucket_owned = bucket.to_string();
    let upload_id_owned = upload_id.to_string();
    let result = web::block(move || {
        for part_key in part_keys {
            store.delete(&user_id_owned, &bucket_owned, &part_key)?;
        }
        store.delete(&user_id_owned, &bucket_owned, &multipart_meta_key(&upload_id_owned))
    })
    .await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => log::warn!("multipart abort cleanup failed for {bucket}/{upload_id}: {e}"),
        Err(e) => log::warn!("multipart abort cleanup blocking task failed for {bucket}/{upload_id}: {e}"),
    }
    Ok(())
}

/// Diagnostic endpoint: phase-level latency breakdown for `cluster_put_object`,
/// so a load test can show where time actually goes (added specifically to
/// answer "where are we slow if we're not saturating anywhere" — see
/// docs/Distributed-Engine-Plan.md).
/// Process-wide cumulative count of shard-backing-store bytes a GET required to
/// serve its response, summed across every content-dependent and plain read
/// this coordinator handled. A workload driver snapshots it before/after a query
/// (see `/cluster/_internal/read_stats`) to measure, end to end through a real
/// engine's own scan, how much less an intra-object-pushdown read pulls off disk.
pub static SHARD_BYTES_READ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Parameters for the read-cost pushdown gate (see `packing::pushdown_read_cost`),
/// parsed once from the environment:
///   WARPDRIVE_PUSHDOWN_SELECTIVITY  expected fraction of an object a query reads
///                                   (default 0.25 = a quarter of the columns)
///   WARPDRIVE_PUSHDOWN_RTT_BYTES    byte-equivalent cost of one extra stripe
///                                   fetch / round trip (default 65536)
/// Env rather than per-bucket config to avoid a config-store schema change; the
/// natural next step is to move both onto `BucketPlacementConfig` so each bucket
/// can reflect its own workload and storage medium.
fn pushdown_cost_params() -> (f64, f64) {
    static PARAMS: std::sync::OnceLock<(f64, f64)> = std::sync::OnceLock::new();
    *PARAMS.get_or_init(|| {
        let sel = std::env::var("WARPDRIVE_PUSHDOWN_SELECTIVITY").ok()
            .and_then(|v| v.parse::<f64>().ok()).filter(|v| *v > 0.0 && *v <= 1.0).unwrap_or(0.25);
        let rtt = std::env::var("WARPDRIVE_PUSHDOWN_RTT_BYTES").ok()
            .and_then(|v| v.parse::<f64>().ok()).filter(|v| *v >= 0.0).unwrap_or(65_536.0);
        (sel, rtt)
    })
}

pub async fn cluster_timing_stats(state: web::Data<ClusterState>) -> HttpResponse {
    HttpResponse::Ok().content_type("text/plain").body(state.timing.summary())
}

/// Cumulative shard bytes read (see `SHARD_BYTES_READ`), as JSON. Monotonic
/// since process start; snapshot the delta around a query to attribute reads.
pub async fn cluster_read_stats() -> HttpResponse {
    let n = SHARD_BYTES_READ.load(std::sync::atomic::Ordering::Relaxed);
    HttpResponse::Ok().content_type("application/json").body(format!("{{\"shard_bytes_read\":{n}}}"))
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

/// Bare, pre-auth route -- see `cluster_put_object`'s matching doc.
pub async fn cluster_get_object(
    path: web::Path<(String, String)>,
    req: actix_web::HttpRequest,
    state: web::Data<ClusterState>,
) -> Result<HttpResponse, Error> {
    let (bucket, key) = path.into_inner();
    cluster_get_object_inner(UNAUTHENTICATED_USER, bucket, key, &req, &state).await
}

/// Shared by both GET read paths (plain and FAC): a reconstructed object
/// must never reach the client if it doesn't match its stored checksum --
/// that's what actually catches post-write corruption (bit-rot, an
/// EC-reconstruction bug, a shard silently corrupted on one peer but
/// still accepted by read-quorum), as opposed to only checking a
/// client-sent checksum once at write time. A record with no stored
/// checksum passes through untouched instead of failing closed: the one
/// real path that produces this today is the bare, unauthenticated
/// `/cluster/{bucket}/{key}` route (`cluster_put_object` passes
/// `ObjectS3Metadata::default()`, which never computes one), not old
/// data from before this feature existed -- this project has no
/// production deployment yet, so there's no such data to be compatible
/// with. Same passthrough for an unrecognized algorithm string.
fn verify_object_checksum(
    bucket: &str, key: &str, data: &[u8], stored_checksum: &Option<(String, String)>,
) -> Result<(), Error> {
    let Some((algo_str, expected)) = stored_checksum else { return Ok(()) };
    let Some(algo) = ChecksumAlgorithm::from_str(algo_str) else { return Ok(()) };
    if crate::s3::handlers::checksum::verify_checksum(&algo, data, expected) {
        Ok(())
    } else {
        Err(ErrorInternalServerError(format!(
            "data integrity check failed for bucket={bucket} key={key}: reconstructed object does not \
             match its stored {algo_str} checksum -- refusing to return possibly-corrupted data"
        )))
    }
}

/// `(header-name, value)` to echo on a GET/HEAD response when the client
/// sent `x-amz-checksum-mode: ENABLED` and the record actually has a
/// stored checksum -- same opt-in convention single-node's
/// `s3_get_object_handler`/`s3_head_object_handler` use.
fn checksum_response_header(
    req: &actix_web::HttpRequest, checksum_algorithm: &Option<String>, checksum_value: &Option<String>,
) -> Option<(String, String)> {
    let enabled = req
        .headers()
        .get("x-amz-checksum-mode")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("ENABLED"));
    if !enabled {
        return None;
    }
    let algo = ChecksumAlgorithm::from_str(checksum_algorithm.as_deref()?)?;
    Some((format!("x-amz-checksum-{}", algo.header_suffix()), checksum_value.clone()?))
}

pub(crate) async fn cluster_get_object_inner(
    user_id: &str,
    bucket: String,
    key: String,
    req: &actix_web::HttpRequest,
    state: &web::Data<ClusterState>,
) -> Result<HttpResponse, Error> {
    // A content-dependent PUT pins into a separate store (different record
    // shape — many stripes, not one shard set). Check there first: cheap,
    // and a key is either content-dependent or not, decided once at PUT
    // time by header presence.
    if let Some(record) = state.content_location_store.get(user_id, &bucket, &key) {
        let total_len = record.original_len as u64;
        // Captured before `record` moves into the fetch calls below --
        // same metadata fields `ClusterMetadataStore::get_metadata` reads,
        // now also surfaced on the actual GET response.
        let etag = record.etag.clone();
        let content_type = record.content_type.clone();
        let last_modified = record.last_modified.clone().unwrap_or_else(super::s3_surface::now_http_date);
        let user_metadata = record.user_metadata.clone();
        let checksum_header = checksum_response_header(req, &record.checksum_algorithm, &record.checksum_value);
        let stored_checksum = record.checksum_algorithm.clone().zip(record.checksum_value.clone());
        let has_sparse_gaps = record.has_sparse_gaps;
        // Range-aware fetch only reaches the stripes whose units actually
        // overlap the requested bytes (see packed.rs) — this is the whole
        // reason a DuckDB-style selective Parquet read benefits from
        // content-dependent placement and a plain object doesn't.
        let range_result = parse_range_header(&req, total_len);
        return match range_result {
            RangeResult::Valid(start, end) => {
                let (data, stat) = super::packed::get_object_content_dependent_range(record, start, end, &state).await?;
                let mut resp = HttpResponse::build(actix_web::http::StatusCode::PARTIAL_CONTENT);
                resp.insert_header(("Accept-Ranges", "bytes"))
                    .insert_header(("Content-Range", format!("bytes {start}-{end}/{total_len}")))
                    .insert_header(("Last-Modified", last_modified));
                if let Some(etag) = &etag { resp.insert_header(("ETag", etag.as_str())); }
                if let Some(ct) = &content_type { resp.insert_header(("Content-Type", ct.as_str())); }
                for (k, v) in &user_metadata {
                    resp.insert_header((format!("x-amz-meta-{k}"), metadata_value_header(v)));
                }
                // Deliberately NOT echoing the object checksum here: the stored
                // value is the FULL-OBJECT checksum, but this is a partial (206)
                // body. A strict client (pyarrow/awscrt) would recompute the
                // checksum over the partial bytes and reject the whole-object
                // value as a mismatch -- exactly what AWS avoids by not
                // returning x-amz-checksum-* on a ranged GET.
                let _ = &checksum_header;
                attach_fetch_stat_headers(&mut resp, &stat);
                if has_sparse_gaps {
                    resp.insert_header(("X-Warpd-Sparse-Coverage", "true"));
                }
                Ok(resp.body(data))
            }
            RangeResult::Unsatisfiable => Err(
                actix_web::error::ErrorRangeNotSatisfiable("the requested range is not valid for this object"),
            ),
            RangeResult::None => {
                let (data, stat) = super::packed::get_object_content_dependent(record, &state).await?;
                verify_object_checksum(&bucket, &key, &data, &stored_checksum)?;

                let mut resp = HttpResponse::Ok();
                resp.insert_header(("Accept-Ranges", "bytes"))
                    .insert_header(("Last-Modified", last_modified));
                if let Some(etag) = &etag { resp.insert_header(("ETag", etag.as_str())); }
                if let Some(ct) = &content_type { resp.insert_header(("Content-Type", ct.as_str())); }
                for (k, v) in &user_metadata {
                    resp.insert_header((format!("x-amz-meta-{k}"), metadata_value_header(v)));
                }
                if let Some((name, value)) = &checksum_header {
                    resp.insert_header((name.as_str(), value.as_str()));
                }
                attach_fetch_stat_headers(&mut resp, &stat);
                if has_sparse_gaps {
                    resp.insert_header(("X-Warpd-Sparse-Coverage", "true"));
                }
                Ok(resp.body(data))
            }
        };
    }

    // Never recompute placement here — look up the pin from write time.
    let record = state
        .location_store
        .get(user_id, &bucket, &key)
        .ok_or_else(|| ErrorNotFound("object not found"))?;
    let total_len = record.original_len as u64;
    let etag = record.etag.clone();
    let content_type = record.content_type.clone();
    let last_modified = record.last_modified.clone().unwrap_or_else(super::s3_surface::now_http_date);
    let user_metadata = record.user_metadata.clone();
    let checksum_header = checksum_response_header(req, &record.checksum_algorithm, &record.checksum_value);

    // #151: an empty `version` means this pin predates the field
    // (`#[serde(default)]`) -- its shards are at the old, unversioned
    // `shard_key(bucket, key, idx)`, so fetch with the bare key in that
    // case rather than a `versioned_key` that was never written.
    let wire_key = if record.version.is_empty() {
        key.clone()
    } else {
        versioned_key(&key, &record.version)
    };

    // A Range request against the plain path used to force a full k+m
    // fan-out and a full EC decode of the whole object before slicing out
    // the requested bytes, no matter how small the range -- every one of
    // DuckDB's many small per-column-chunk range-GETs paid the full
    // object's fetch and decode cost. Reed-Solomon here is systematic
    // (ec.rs: `encode` only computes parity from the k data shards, never
    // rewrites them), so when the data shards covering the requested
    // range are all healthy, no decode is needed at all -- just fetch
    // those specific data shards and slice. Falls back to the original
    // full fetch+decode path (below) if any needed shard isn't
    // available, so correctness under failures is unchanged.
    if let RangeResult::Valid(start, end) = parse_range_header(&req, total_len) {
        let shard_len = ((record.original_len + record.k - 1) / record.k).max(1);
        let first_idx = ((start as usize) / shard_len).min(record.k - 1);
        let last_idx = ((end as usize) / shard_len).min(record.k - 1);

        let partial_gets = (first_idx..=last_idx).map(|idx| {
            let client = Arc::clone(&state.peer_client);
            let bucket = bucket.clone();
            let wire_key = wire_key.clone();
            let peer = record.shard_peers[idx].clone();
            async move { client.get_shard(&peer, user_id, &bucket, &wire_key, idx).await.ok() }
        });
        let partial_results = join_all(partial_gets).await;

        if partial_results.iter().all(|r| r.is_some()) {
            let mut buf = Vec::with_capacity((last_idx - first_idx + 1) * shard_len);
            for shard in partial_results.into_iter().flatten() {
                buf.extend_from_slice(&shard);
            }
            let local_start = start as usize - first_idx * shard_len;
            let local_end = (end as usize - first_idx * shard_len).min(buf.len() - 1);
            if let Some(slice) = buf.get(local_start..=local_end) {
                let mut resp = HttpResponse::build(actix_web::http::StatusCode::PARTIAL_CONTENT);
                resp.insert_header(("Accept-Ranges", "bytes"))
                    .insert_header(("Content-Range", format!("bytes {start}-{end}/{total_len}")))
                    .insert_header(("Last-Modified", last_modified.clone()));
                if let Some(etag) = &etag { resp.insert_header(("ETag", etag.as_str())); }
                if let Some(ct) = &content_type { resp.insert_header(("Content-Type", ct.as_str())); }
                for (k, v) in &user_metadata {
                    resp.insert_header((format!("x-amz-meta-{k}"), metadata_value_header(v)));
                }
                // No object checksum on a partial (206) body -- it's the
                // whole-object value and a strict client would reject it
                // against the partial bytes (see the content-dependent
                // partial arm above for the full rationale).
                let _ = &checksum_header;
                // Plain-path read telemetry, comparable to the content-dependent
                // path's: a plain object is one opaque blob split into `k`
                // positional shards, so a range still reads every data shard it
                // spans (here first_idx..=last_idx), each `shard_len` bytes --
                // with no per-column structure to be more selective about.
                let plain_bytes = ((last_idx - first_idx + 1) * shard_len) as u64;
                resp.insert_header(("x-warpd-placement", "plain"));
                resp.insert_header(("x-warpd-shard-bytes-read", plain_bytes.to_string()));
                SHARD_BYTES_READ.fetch_add(plain_bytes, std::sync::atomic::Ordering::Relaxed);
                return Ok(resp.body(slice.to_vec()));
            }
        }
        // Fall through to the full fetch+decode path below -- a needed
        // data shard's peer was unavailable, or the slice math above
        // came up empty; both are rare-path, not the common case this
        // fix targets.
    }

    let gets = record.shard_peers.iter().cloned().enumerate().map(|(idx, peer)| {
        let client = Arc::clone(&state.peer_client);
        let bucket = bucket.clone();
        let wire_key = wire_key.clone();
        async move {
            match client.get_shard(&peer, user_id, &bucket, &wire_key, idx).await {
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

    // Verify the reconstructed object against its stored checksum before
    // ever returning it -- every object has one by default now (see
    // `cluster_s3_put_object_authenticated`'s matching comment), not just
    // objects whose client explicitly requested AWS's checksum feature.
    // This is what actually catches post-write corruption (bit-rot, a
    // reconstruction bug, a shard silently corrupted on one peer but
    // still accepted by read-quorum) -- storing a checksum at write time
    // is not a safety guarantee by itself if nothing ever checks it
    // again on read. A mismatch means the data this node just
    // reconstructed is not what was written, so it must never reach the
    // client -- fail the request instead of serving it.
    let stored_checksum = record.checksum_algorithm.clone().zip(record.checksum_value.clone());
    verify_object_checksum(&bucket, &key, &data, &stored_checksum)?;

    match parse_range_header(&req, total_len) {
        RangeResult::Valid(start, end) => {
            let slice = data
                .get(start as usize..=end as usize)
                .ok_or_else(|| actix_web::error::ErrorRangeNotSatisfiable("range out of bounds"))?
                .to_vec();
            let mut resp = HttpResponse::build(actix_web::http::StatusCode::PARTIAL_CONTENT);
            resp.insert_header(("Accept-Ranges", "bytes"))
                .insert_header(("Content-Range", format!("bytes {start}-{end}/{total_len}")))
                .insert_header(("Last-Modified", last_modified));
            if let Some(etag) = &etag { resp.insert_header(("ETag", etag.as_str())); }
            if let Some(ct) = &content_type { resp.insert_header(("Content-Type", ct.as_str())); }
            for (k, v) in &user_metadata {
                resp.insert_header((format!("x-amz-meta-{k}"), metadata_value_header(v)));
            }
            // No object checksum on a partial (206) body (see the fast-path
            // partial arm above): it's the whole-object value.
            let _ = &checksum_header;
            // This arm is only reached when the shard fast-path above fell
            // through and we decoded the WHOLE object to slice from it, so the
            // plain path read every data shard regardless of range size.
            resp.insert_header(("x-warpd-placement", "plain"));
            resp.insert_header(("x-warpd-shard-bytes-read", total_len.to_string()));
            SHARD_BYTES_READ.fetch_add(total_len, std::sync::atomic::Ordering::Relaxed);
            Ok(resp.body(slice))
        }
        RangeResult::Unsatisfiable => Err(
            actix_web::error::ErrorRangeNotSatisfiable("the requested range is not valid for this object"),
        ),
        RangeResult::None => {
            let mut resp = HttpResponse::Ok();
            resp.insert_header(("Accept-Ranges", "bytes"))
                .insert_header(("Last-Modified", last_modified));
            if let Some(etag) = &etag { resp.insert_header(("ETag", etag.as_str())); }
            if let Some(ct) = &content_type { resp.insert_header(("Content-Type", ct.as_str())); }
            for (k, v) in &user_metadata {
                resp.insert_header((format!("x-amz-meta-{k}"), metadata_value_header(v)));
            }
            if let Some((name, value)) = &checksum_header {
                resp.insert_header((name.as_str(), value.as_str()));
            }
            resp.insert_header(("x-warpd-placement", "plain"));
            resp.insert_header(("x-warpd-shard-bytes-read", total_len.to_string()));
            SHARD_BYTES_READ.fetch_add(total_len, std::sync::atomic::Ordering::Relaxed);
            Ok(resp.body(data))
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
        .get(UNAUTHENTICATED_USER, &bucket, &key)
        .ok_or_else(|| ErrorNotFound("no content-dependent record for this key"))?;
    Ok(HttpResponse::Ok().json(record))
}

/// Bare, pre-auth route -- see `cluster_put_object`'s matching doc.
pub async fn cluster_delete_object(
    path: web::Path<(String, String)>,
    state: web::Data<ClusterState>,
) -> Result<HttpResponse, Error> {
    let (bucket, key) = path.into_inner();
    cluster_delete_object_inner(UNAUTHENTICATED_USER, bucket, key, &state).await
}

pub(crate) async fn cluster_delete_object_inner(
    user_id: &str,
    bucket: String,
    key: String,
    state: &web::Data<ClusterState>,
) -> Result<HttpResponse, Error> {
    // A key is either plain (`LocationStore`) or content-dependent/packed
    // (`ContentLocationStore`) — decided once, at PUT time, by which path
    // handled it. A key can even have a *stale* entry in the other store
    // left over from an earlier overwrite that changed which path it took
    // (see `put_object_plain`'s and `put_object_content_dependent`'s
    // overwrite handling).
    let plain_record = state.location_store.get(user_id, &bucket, &key);
    let packed_record = state.content_location_store.get(user_id, &bucket, &key);

    // Deliberately *not* a 404 short-circuit when both are locally absent:
    // this coordinator only needed quorum to receive either pin, not all
    // of it, so it can have neither locally while the object is still
    // genuinely readable from other peers. Returning 404 here would leave
    // those peers' copies untouched forever. Broadcast both tombstones
    // unconditionally below instead (idempotent on a receiver with
    // nothing to delete) and let that be authoritative, the same fix
    // applied to the overwrite-cleanup paths above (see issue #153).
    // Matches real S3 semantics too: DELETE is idempotent, not an error on
    // a key that doesn't exist.

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

    // k/m for the quorum-ack threshold: whichever record this node
    // happens to have locally (either is fine, they're cluster-wide
    // constants in practice), falling back to this node's own default if
    // it has neither locally, same fallback `put_object_plain` uses to
    // size a fresh write.
    let (k, m) = plain_record
        .as_ref()
        .map(|r| (r.k, r.m))
        .or_else(|| packed_record.as_ref().map(|r| (r.k, r.m)))
        .unwrap_or_else(|| (state.ec.k(), state.ec.m()));
    let required_acks = required_write_acks(k, m);

    let plain_acked = replicate_location_delete(state, user_id, &bucket, &key, &peers).await;
    let packed_acked = replicate_content_location_delete(state, user_id, &bucket, &key, &peers).await;
    if plain_acked < required_acks || packed_acked < required_acks {
        return Err(ErrorInternalServerError(format!(
            "tombstone quorum not met: plain {plain_acked}/{required_acks}, \
             packed {packed_acked}/{required_acks} peers acknowledged"
        )));
    }

    // Real S3 semantics (and real S3 SDKs, e.g. minio-go used by
    // minio/warp): a successful DeleteObject returns 204 No Content, not
    // 200 -- the SDK client treats any other status as an error even
    // though the delete itself succeeded.
    Ok(HttpResponse::NoContent().finish())
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

    // Explicit, not just a fallthrough to "object not found": a packed
    // object might genuinely exist (in content_location_store) while
    // simply having no retention support -- that's a different, clearer
    // error than claiming the key doesn't exist at all.
    if state.content_location_store.get(UNAUTHENTICATED_USER, &bucket, &key).is_some() {
        return Err(packed_object_lock_not_implemented());
    }

    let mut record = state
        .location_store
        .get(UNAUTHENTICATED_USER, &bucket, &key)
        .ok_or_else(|| ErrorNotFound("object not found"))?;

    record.retention_mode = Some(body.mode.clone());
    record.retain_until = Some(body.retain_until.clone());
    record.legal_hold = body.legal_hold;

    // #156 (point 2): this used to replicate only to `record.shard_peers`
    // (a quorum subset) -- but cluster_delete_object enforces retention by
    // checking whatever plain record *this* coordinator happens to have
    // locally, and any peer can end up coordinating a DELETE, not just
    // shard holders. A peer outside `shard_peers` that received the
    // object's original PUT (which does replicate to every peer, see
    // put_object_plain) kept its stale, pre-lock copy of the record
    // forever, so a DELETE routed through it saw no retention_mode at all
    // and succeeded. Replicating to every peer, and requiring every peer
    // to ack -- same reasoning and same pattern as
    // cluster_put_bucket_config just above: this is a fact every
    // coordinator must agree on, not a write quorum is enough for.
    let peers = state.membership.peers();
    let acked = replicate_location_put(&state, &record, &peers).await;
    if acked < peers.len() {
        return Err(ErrorInternalServerError(format!(
            "retention update requires every peer to ack: {acked}/{} acknowledged",
            peers.len()
        )));
    }

    Ok(HttpResponse::Ok().finish())
}

// ---------------------------------------------------------------------------
// Phase 4: object tagging, authenticated-surface support functions.
// ---------------------------------------------------------------------------

/// Sets an object's tags, replicated to every peer (same all-peers-ack
/// reasoning as `cluster_put_retention` -- any peer can coordinate a
/// later GET/DELETE, so every peer must agree on the new tag set, not
/// just a write-quorum subset). Unlike retention, tagging applies to
/// both plain and content-dependent (packed) records -- tags are a pure
/// metadata field on either record shape, with no EC/placement
/// implications, so there's no reason to restrict it to plain objects.
pub(crate) async fn cluster_put_object_tagging_inner(
    user_id: &str, bucket: &str, key: &str, tags: Vec<(String, String)>, state: &web::Data<ClusterState>,
) -> Result<HttpResponse, Error> {
    let peers = state.membership.peers();

    if let Some(mut record) = state.content_location_store.get(user_id, bucket, key) {
        record.tags = tags;
        let acked = replicate_content_location_put(state, &record, &peers).await;
        if acked < peers.len() {
            return Err(ErrorInternalServerError(format!(
                "tagging update requires every peer to ack: {acked}/{} acknowledged", peers.len()
            )));
        }
        return Ok(HttpResponse::Ok().insert_header(("Content-Length", "0")).body(""));
    }

    if let Some(mut record) = state.location_store.get(user_id, bucket, key) {
        record.tags = tags;
        let acked = replicate_location_put(state, &record, &peers).await;
        if acked < peers.len() {
            return Err(ErrorInternalServerError(format!(
                "tagging update requires every peer to ack: {acked}/{} acknowledged", peers.len()
            )));
        }
        return Ok(HttpResponse::Ok().insert_header(("Content-Length", "0")).body(""));
    }

    Err(ErrorNotFound("object not found"))
}

/// Reads an object's tags -- a pure local lookup, same precedence
/// (content-dependent checked first) as every other cluster GET-side
/// read. Not async: no network call needed.
pub(crate) fn cluster_get_object_tagging(
    user_id: &str, bucket: &str, key: &str, state: &web::Data<ClusterState>,
) -> Result<Vec<(String, String)>, Error> {
    if let Some(record) = state.content_location_store.get(user_id, bucket, key) {
        return Ok(record.tags);
    }
    if let Some(record) = state.location_store.get(user_id, bucket, key) {
        return Ok(record.tags);
    }
    Err(ErrorNotFound("object not found"))
}

/// Clears an object's tags -- same shape as the put side above, just an
/// empty tag set.
pub(crate) async fn cluster_delete_object_tagging_inner(
    user_id: &str, bucket: &str, key: &str, state: &web::Data<ClusterState>,
) -> Result<HttpResponse, Error> {
    cluster_put_object_tagging_inner(user_id, bucket, key, Vec::new(), state).await
}

// ---------------------------------------------------------------------------
// Phase 4: object-lock (retention/legal-hold), authenticated-surface
// support functions. Plain records only -- `ContentDependentRecord` has
// no retention/legal_hold fields at all (see `cluster_put_retention`'s
// own doc on why object-lock was never extended to packed objects); a
// caller targeting a packed key gets a clear, explicit error instead of
// a silent no-op that would let a packed object bypass locking entirely.
// ---------------------------------------------------------------------------

fn packed_object_lock_not_implemented() -> Error {
    ErrorInternalServerError(
        "object-lock (retention/legal-hold) is not implemented for content-dependent \
         (packed) objects -- a documented, carried-forward gap, not a silent no-op",
    )
}

/// Sets *only* `retention_mode`/`retain_until`, leaving `legal_hold`
/// untouched -- the real S3 API treats PutObjectRetention and
/// PutObjectLegalHold as independent calls; `cluster_put_retention`
/// (above) is the bare, pre-auth, sentinel-only endpoint that sets both
/// together and predates this split. Authenticated callers use this
/// instead.
pub(crate) async fn cluster_put_object_retention_inner(
    user_id: &str, bucket: &str, key: &str, mode: &str, retain_until: &str, state: &web::Data<ClusterState>,
) -> Result<HttpResponse, Error> {
    if mode != "GOVERNANCE" && mode != "COMPLIANCE" {
        return Err(ErrorBadRequest("mode must be GOVERNANCE or COMPLIANCE"));
    }
    if state.content_location_store.get(user_id, bucket, key).is_some() {
        return Err(packed_object_lock_not_implemented());
    }
    let mut record = state
        .location_store
        .get(user_id, bucket, key)
        .ok_or_else(|| ErrorNotFound("object not found"))?;
    record.retention_mode = Some(mode.to_string());
    record.retain_until = Some(retain_until.to_string());

    let peers = state.membership.peers();
    let acked = replicate_location_put(state, &record, &peers).await;
    if acked < peers.len() {
        return Err(ErrorInternalServerError(format!(
            "retention update requires every peer to ack: {acked}/{} acknowledged", peers.len()
        )));
    }
    Ok(HttpResponse::Ok().insert_header(("Content-Length", "0")).body(""))
}

/// Sets *only* `legal_hold`, leaving `retention_mode`/`retain_until`
/// untouched -- the real S3 API treats PutObjectLegalHold and
/// PutObjectRetention as independent calls; `cluster_put_retention`
/// (above) sets both together and predates this split.
pub(crate) async fn cluster_put_object_legal_hold_inner(
    user_id: &str, bucket: &str, key: &str, legal_hold: bool, state: &web::Data<ClusterState>,
) -> Result<HttpResponse, Error> {
    if state.content_location_store.get(user_id, bucket, key).is_some() {
        return Err(packed_object_lock_not_implemented());
    }
    let mut record = state
        .location_store
        .get(user_id, bucket, key)
        .ok_or_else(|| ErrorNotFound("object not found"))?;
    record.legal_hold = legal_hold;

    let peers = state.membership.peers();
    let acked = replicate_location_put(state, &record, &peers).await;
    if acked < peers.len() {
        return Err(ErrorInternalServerError(format!(
            "legal-hold update requires every peer to ack: {acked}/{} acknowledged", peers.len()
        )));
    }
    Ok(HttpResponse::Ok().insert_header(("Content-Length", "0")).body(""))
}

/// Returns `(mode, retain_until)` if the plain object has an active
/// retention record, `Ok(None)` if it exists but has none set, `Err` if
/// the key doesn't exist or resolves to a packed object.
pub(crate) fn cluster_get_object_retention(
    user_id: &str, bucket: &str, key: &str, state: &web::Data<ClusterState>,
) -> Result<Option<(String, String)>, Error> {
    if state.content_location_store.get(user_id, bucket, key).is_some() {
        return Err(packed_object_lock_not_implemented());
    }
    let record = state
        .location_store
        .get(user_id, bucket, key)
        .ok_or_else(|| ErrorNotFound("object not found"))?;
    Ok(match (record.retention_mode, record.retain_until) {
        (Some(mode), Some(until)) => Some((mode, until)),
        _ => None,
    })
}

/// Returns whether legal hold is currently set on the plain object.
pub(crate) fn cluster_get_object_legal_hold(
    user_id: &str, bucket: &str, key: &str, state: &web::Data<ClusterState>,
) -> Result<bool, Error> {
    if state.content_location_store.get(user_id, bucket, key).is_some() {
        return Err(packed_object_lock_not_implemented());
    }
    let record = state
        .location_store
        .get(user_id, bucket, key)
        .ok_or_else(|| ErrorNotFound("object not found"))?;
    Ok(record.legal_hold)
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
    // `put` now does a real (group-committed) fsync -- see
    // `storage::group_commit`'s module doc. Calling it directly here
    // would block this actix worker's whole event loop for the fsync's
    // duration, which also starves group commit of the concurrent
    // callers it needs to batch well (measured locally: throughput fell
    // ~99% without this). `web::block` matches what single-node's own
    // PUT path (`s3/handlers/object.rs`) already does for the same
    // underlying reason.
    let store = Arc::clone(&state.content_location_store);
    let record = body.into_inner();
    web::block(move || store.put(record).map_err(|e| e.to_string()))
        .await
        .map_err(|e| ErrorInternalServerError(format!("blocking task failed: {e}")))?
        .map_err(ErrorInternalServerError)?;
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
    // See `cluster_internal_put_content_location`'s matching comment.
    let store = Arc::clone(&state.location_store);
    let record = body.into_inner();
    web::block(move || store.put(record).map_err(|e| e.to_string()))
        .await
        .map_err(|e| ErrorInternalServerError(format!("blocking task failed: {e}")))?
        .map_err(ErrorInternalServerError)?;
    Ok(HttpResponse::Ok().finish())
}

/// `(bucket, key)` for the internal tombstone-replication endpoints,
/// sent as a JSON body rather than URL path segments (#149): actix-web
/// fully percent-*decodes* path parameters, so a key containing `?`,
/// `#`, or `%` that was embedded unencoded into a path (as this used to
/// be, via `format!(".../{bucket}/{key}")`) could be reinterpreted as a
/// query string, a fragment, or a different literal byte sequence by
/// the time it reached the handler -- silently deleting the pin for a
/// *different* key than the one that was actually meant. A JSON body
/// has no such reinterpretation step; this is exactly the same
/// path-vs-body choice the PUT side of both these stores already made
/// (`cluster_internal_put_location`/`_content_location` take
/// `web::Json<LocationRecord>`/`<ContentDependentRecord>`, never a path
/// param), just applied consistently to DELETE too.
#[derive(Debug, Serialize, Deserialize)]
pub struct DeleteKeyRequest {
    /// `#[serde(default)]`: a tombstone sent by an older peer process that
    /// predates this field replays as `UNAUTHENTICATED_USER`, matching
    /// what `LocationRecord`/`ContentDependentRecord`'s own `user_id`
    /// default to for the same reason.
    #[serde(default)]
    pub user_id: String,
    pub bucket: String,
    pub key: String,
}

/// Receiving side of tombstone replication.
pub async fn cluster_internal_delete_location(
    body: web::Json<DeleteKeyRequest>,
    state: web::Data<ClusterState>,
) -> Result<HttpResponse, Error> {
    // See `cluster_internal_put_content_location`'s matching comment:
    // `delete` fsyncs a tombstone append too.
    let store = Arc::clone(&state.location_store);
    let DeleteKeyRequest { user_id, bucket, key } = body.into_inner();
    web::block(move || store.delete(&user_id, &bucket, &key).map_err(|e| e.to_string()))
        .await
        .map_err(|e| ErrorInternalServerError(format!("blocking task failed: {e}")))?
        .map_err(ErrorInternalServerError)?;
    Ok(HttpResponse::Ok().finish())
}

/// Receiving side of content-location tombstone replication — the
/// multi-stripe counterpart to `cluster_internal_delete_location` above.
pub async fn cluster_internal_delete_content_location(
    body: web::Json<DeleteKeyRequest>,
    state: web::Data<ClusterState>,
) -> Result<HttpResponse, Error> {
    let store = Arc::clone(&state.content_location_store);
    let DeleteKeyRequest { user_id, bucket, key } = body.into_inner();
    web::block(move || store.delete(&user_id, &bucket, &key).map_err(|e| e.to_string()))
        .await
        .map_err(|e| ErrorInternalServerError(format!("blocking task failed: {e}")))?
        .map_err(ErrorInternalServerError)?;
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

    #[test]
    fn decode_aws_chunked_payload_strips_framing_to_the_real_bytes() {
        // Exact shape captured from a real minio-go (warp) PUT: two data
        // chunks of different sizes, then a zero-size terminal chunk.
        let mut raw = Vec::new();
        raw.extend_from_slice(b"5;chunk-signature=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\r\n");
        raw.extend_from_slice(b"hello");
        raw.extend_from_slice(b"\r\n");
        raw.extend_from_slice(b"6;chunk-signature=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\r\n");
        raw.extend_from_slice(b" world");
        raw.extend_from_slice(b"\r\n");
        raw.extend_from_slice(b"0;chunk-signature=cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc\r\n");
        raw.extend_from_slice(b"\r\n");

        let decoded = decode_aws_chunked_payload(&raw).unwrap();
        assert_eq!(decoded, b"hello world");
    }

    #[test]
    fn decode_aws_chunked_payload_handles_single_chunk_matching_real_capture() {
        // Same chunk size (0x10000 = 65536) seen in the real capture.
        let mut raw = Vec::new();
        let data = vec![0xABu8; 0x10000];
        raw.extend_from_slice(b"10000;chunk-signature=deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef\r\n");
        raw.extend_from_slice(&data);
        raw.extend_from_slice(b"\r\n");
        raw.extend_from_slice(b"0;chunk-signature=deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef\r\n");
        raw.extend_from_slice(b"\r\n");

        let decoded = decode_aws_chunked_payload(&raw).unwrap();
        assert_eq!(decoded.len(), 0x10000);
        assert_eq!(decoded, data);
    }

    #[test]
    fn decode_aws_chunked_payload_rejects_truncated_chunk() {
        let raw = b"a;chunk-signature=abc\r\nshort\r\n".to_vec();
        assert!(decode_aws_chunked_payload(&raw).is_err());
    }

    /// The real safety property added this release: a reconstructed
    /// object that doesn't match its stored checksum must never reach
    /// the client. Tested directly against `verify_object_checksum`
    /// (shared by the plain and FAC GET paths) rather than through a live
    /// multi-node cluster -- round-tripping actual EC-encoded bytes
    /// through real peer servers belongs to the Python S3-compat suite
    /// (`server/tests/python/s3_boto_test.py`, run against both `/s3` and
    /// `/cluster/s3`); this is the fast, deterministic unit coverage for
    /// the verification logic itself, independent of transport/placement.
    #[test]
    fn verify_object_checksum_accepts_matching_data() {
        let data = b"the real bytes that were written".to_vec();
        let checksum = crate::s3::handlers::checksum::compute_checksum(&ChecksumAlgorithm::Crc32c, &data);
        let stored = Some((ChecksumAlgorithm::Crc32c.as_str().to_string(), checksum));
        assert!(verify_object_checksum("b", "k", &data, &stored).is_ok());
    }

    #[test]
    fn verify_object_checksum_rejects_corrupted_data() {
        let original = b"the real bytes that were written".to_vec();
        let checksum = crate::s3::handlers::checksum::compute_checksum(&ChecksumAlgorithm::Crc32c, &original);
        let stored = Some((ChecksumAlgorithm::Crc32c.as_str().to_string(), checksum));

        // Simulates a shard silently corrupted on disk (bit-rot) or a
        // reconstruction bug: the bytes handed to the client-facing
        // response are not the bytes the checksum was computed over.
        let mut corrupted = original.clone();
        corrupted[10] ^= 0xFF;
        assert_ne!(corrupted, original);

        let err = verify_object_checksum("b", "k", &corrupted, &stored)
            .expect_err("corrupted data must be rejected, not served");
        let msg = err.to_string();
        assert!(msg.contains("data integrity check failed"), "unexpected error message: {msg}");
    }

    #[test]
    fn verify_object_checksum_passes_through_for_the_bare_unauthenticated_route() {
        // `/cluster/{bucket}/{key}` never computes a checksum
        // (`ObjectS3Metadata::default()`) -- must not fail closed on data
        // nothing ever checksummed.
        let data = b"object written through the bare pre-auth route".to_vec();
        assert!(verify_object_checksum("b", "k", &data, &None).is_ok());
    }

    /// #149: `replicate_location_delete`/`replicate_content_location_delete`
    /// now send `(bucket, key)` as this JSON body instead of interpolating
    /// them into a URL path, specifically so a key containing `?`, `#`,
    /// `%`, space, or `/` reaches the receiving peer's handler as the
    /// exact bytes the client meant -- a JSON string has no character
    /// that a URL parser would ever reinterpret as a delimiter.
    #[test]
    fn delete_key_request_round_trips_keys_with_url_special_characters() {
        for key in ["victim?x", "victim#x", "victim%3Fx", "a b", "a/b/c", "100%"] {
            let req = DeleteKeyRequest { user_id: "u1".to_string(), bucket: "b".to_string(), key: key.to_string() };
            let json = serde_json::to_string(&req).unwrap();
            let decoded: DeleteKeyRequest = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded.key, key, "round-trip mismatch for key {key:?}");
        }
    }
}
