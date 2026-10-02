//! Fine-grained query pushdown on FAC-encoded objects, reproducing the
//! mechanism from Fusion's own paper (ASPLOS'25 §4.3): a predicate is
//! evaluated in-situ, on the single storage node holding the relevant
//! computable unit's data block, instead of reassembling the object
//! across every node that holds a piece of it.
//!
//! This works because of a property already true of `ErasureCoder::
//! encode_shards` (systematic Reed-Solomon): shard indices `0..k` ARE the
//! plaintext bins, unmodified — only shards `k..k+m` are coded parity.
//! So the peer holding data-shard `bin_index` already has the real,
//! decoded bytes on local disk; no EC reconstruction is needed to filter
//! it, exactly matching Fusion's claim that pushdown avoids cross-node
//! reassembly for data (not parity) blocks.
//!
//! `ColumnCodec` is the contract (one real implementation, `ZlibF64Codec`,
//! shipped now) a pushdown-capable unit's bytes are decoded with — the
//! same "trait first, one concrete implementation" pattern as
//! `StripePacker`/`ErasureCoder`/`PlacementPolicy`. A unit built without
//! codec info (`codec: "opaque"`, the plain storage/placement path) simply
//! isn't pushdown-capable; `coordinator.rs`'s cost equation naturally
//! treats its compressibility as 1.0, so pushdown is still never *wrong*
//! for such units, it's just not better than the baseline. Swapping in a
//! real Apache Parquet page decoder later (to filter genuine dictionary/
//! RLE-encoded column chunks instead of this project's own simple
//! zlib+f64 columnar format) is a second `ColumnCodec` implementation
//! registered under a new name — no change to this module's orchestration.
//!
//! Scope, matching the microbenchmark-only decision in
//! docs/Distributed-Engine-Plan.md: filter and projection happen in one
//! round trip (the microbenchmark's query targets the same column for
//! both WHERE and SELECT, so there is nothing to gain from splitting them
//! into two stages yet). The two-stage filter-then-project split Fusion
//! uses for multi-column queries (Q1-Q4) is deferred along with those
//! queries.

use actix_web::error::{ErrorBadRequest, ErrorInternalServerError, ErrorNotFound};
use actix_web::{web, Error, HttpResponse};
use serde::{Deserialize, Serialize};
use std::io::Read;

use super::coordinator::ClusterState;

pub trait ColumnCodec: Send + Sync {
    /// Decodes a unit's stored bytes into a flat list of f64 values, one
    /// per logical row. The one shipped implementation is `ZlibF64Codec`;
    /// a real Parquet-page codec is a second implementation away, not a
    /// rewrite of anything that calls this trait.
    fn decode(&self, raw: &[u8]) -> Result<Vec<f64>, String>;
}

/// zlib (DEFLATE) over a little-endian f64 array. Not literally Parquet's
/// own encodings (dictionary + bit-packing + Snappy) — a real, standard,
/// genuinely decodable stand-in chosen specifically so the Python workload
/// driver needs no dependency beyond the stdlib `zlib` module (same reason
/// `requests` was dropped for `urllib` earlier in this project). It still
/// gives real, controllable compression ratios, which is what the cost
/// equation actually depends on.
pub struct ZlibF64Codec;

impl ColumnCodec for ZlibF64Codec {
    fn decode(&self, raw: &[u8]) -> Result<Vec<f64>, String> {
        let mut decoder = flate2::read::ZlibDecoder::new(raw);
        let mut buf = Vec::new();
        decoder.read_to_end(&mut buf).map_err(|e| e.to_string())?;
        if buf.len() % 8 != 0 {
            return Err(format!("decoded byte length {} is not a multiple of 8", buf.len()));
        }
        Ok(buf.chunks_exact(8).map(|c| f64::from_le_bytes(c.try_into().unwrap())).collect())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompareOp {
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
}

fn matches(op: CompareOp, v: f64, value: f64) -> bool {
    match op {
        CompareOp::Lt => v < value,
        CompareOp::Le => v <= value,
        CompareOp::Gt => v > value,
        CompareOp::Ge => v >= value,
        CompareOp::Eq => v == value,
    }
}

/// A column chunk's compressibility, as Fusion defines it (§4.3):
/// uncompressed size over compressed (stored) size.
pub fn compressibility(uncompressed_len: u64, len: u64) -> f64 {
    if len == 0 {
        return 1.0;
    }
    uncompressed_len as f64 / len as f64
}

/// Fusion's Cost Equation (§4.3): push the projection's result down (send
/// only the matching, uncompressed values) only when
/// `selectivity * compressibility < 1`, i.e. only when that's expected to
/// be smaller than the alternative of shipping the whole compressed chunk
/// over the network for the coordinator to decode and filter itself.
pub fn should_push_down_projection(selectivity: f64, compressibility: f64) -> bool {
    selectivity * compressibility < 1.0
}

#[derive(Debug, Deserialize)]
pub struct PushdownQueryBody {
    pub unit_id: String,
    pub op: CompareOp,
    pub value: f64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PushdownQueryResponse {
    pub total_count: usize,
    pub matched_count: usize,
    pub selectivity: f64,
    pub compressibility: f64,
    /// Populated only when `projection_pushed_down` is true; otherwise the
    /// caller must pull and decode the whole chunk itself to get values
    /// (the baseline path `get_object_content_dependent` already provides).
    pub values: Vec<f64>,
    pub projection_pushed_down: bool,
}

/// The internal, peer-to-peer request: fully self-contained (byte range,
/// codec name, predicate) so the owning peer never needs to look up the
/// `ContentDependentRecord` itself — the coordinator already did that once.
#[derive(Debug, Serialize, Deserialize)]
pub struct PushdownFilterInternalRequest {
    pub bucket: String,
    pub key: String,
    pub stripe_index: usize,
    pub bin_index: usize,
    pub offset_in_bin: usize,
    pub unit_len: usize,
    pub uncompressed_len: u64,
    pub codec: String,
    pub op: CompareOp,
    pub value: f64,
}

fn evaluate(raw_bin: &[u8], req: &PushdownFilterInternalRequest, state: &ClusterState) -> Result<PushdownQueryResponse, Error> {
    let unit_bytes = raw_bin
        .get(req.offset_in_bin..req.offset_in_bin + req.unit_len)
        .ok_or_else(|| ErrorInternalServerError("unit byte range out of bounds of stored bin"))?;
    let codec = state
        .column_codecs
        .get(&req.codec)
        .ok_or_else(|| ErrorBadRequest(format!("unknown column codec {:?}", req.codec)))?;
    let values = codec.decode(unit_bytes).map_err(ErrorInternalServerError)?;

    let total_count = values.len();
    let matched: Vec<f64> = values.into_iter().filter(|&v| matches(req.op, v, req.value)).collect();
    let matched_count = matched.len();
    let selectivity = if total_count == 0 { 0.0 } else { matched_count as f64 / total_count as f64 };
    let compress = compressibility(req.uncompressed_len, req.unit_len as u64);
    let push = should_push_down_projection(selectivity, compress);

    Ok(PushdownQueryResponse {
        total_count,
        matched_count,
        selectivity,
        compressibility: compress,
        values: if push { matched } else { Vec::new() },
        projection_pushed_down: push,
    })
}

/// Peer-local handler: runs on whichever node actually stores the relevant
/// bin. Reads just that one shard from local disk (no network fan-out to
/// other stripe peers — the whole point) and evaluates the predicate.
pub async fn cluster_internal_pushdown_filter(
    body: web::Json<PushdownFilterInternalRequest>,
    state: web::Data<ClusterState>,
) -> Result<HttpResponse, Error> {
    let req = body.into_inner();
    let sk = super::packed::stripe_key(&req.key, req.stripe_index);
    let raw_bin = super::shard_storage::load_shard(&req.bucket, &sk, req.bin_index)?;
    let response = evaluate(&raw_bin, &req, &state)?;
    Ok(HttpResponse::Ok().json(response))
}

/// Coordinator-side handler: any node can receive a query (symmetric, like
/// every other cluster endpoint). Looks up the already-replicated
/// `ContentDependentRecord` locally, finds the single peer holding the
/// requested unit's bin, and forwards just that one request — this one hop
/// is the entire network cost being measured, vs. the baseline path's
/// fan-out to every stripe peer plus full-object EC decode.
pub async fn cluster_pushdown_query(
    path: web::Path<(String, String)>,
    body: web::Json<PushdownQueryBody>,
    state: web::Data<ClusterState>,
) -> Result<HttpResponse, Error> {
    let (bucket, key) = path.into_inner();
    let body = body.into_inner();

    let record = state
        .content_location_store
        .get(&bucket, &key)
        .ok_or_else(|| ErrorNotFound("no content-dependent record for this key"))?;

    let (stripe_idx, bin_idx) = record
        .locate_unit(&body.unit_id)
        .ok_or_else(|| ErrorNotFound("unit_id not found in this object's record"))?;

    let unit = record
        .units
        .iter()
        .find(|u| u.unit_id == body.unit_id)
        .expect("locate_unit found this unit_id, so it must be in record.units");

    // Units are stored in a bin in pack order (see StripeRecord::bins) —
    // walk the preceding ones to find this unit's byte offset within the
    // bin's concatenated layout, same walk `get_object_content_dependent`
    // already does for a full reconstruction.
    let bin_unit_ids = &record.stripes[stripe_idx].bins[bin_idx];
    let mut offset_in_bin = 0usize;
    for uid in bin_unit_ids {
        if uid == &body.unit_id {
            break;
        }
        let preceding = record
            .units
            .iter()
            .find(|u| &u.unit_id == uid)
            .ok_or_else(|| ErrorInternalServerError("unit id missing from record's unit index"))?;
        offset_in_bin += preceding.len as usize;
    }

    let peer = record.stripes[stripe_idx].shard_peers[bin_idx].clone();
    let internal_req = PushdownFilterInternalRequest {
        bucket: bucket.clone(),
        key: key.clone(),
        stripe_index: stripe_idx,
        bin_index: bin_idx,
        offset_in_bin,
        unit_len: unit.len as usize,
        uncompressed_len: unit.uncompressed_len,
        codec: unit.codec.clone(),
        op: body.op,
        value: body.value,
    };

    let url = format!("{}/cluster/_internal/pushdown_filter", peer.trim_end_matches('/'));
    let resp = state
        .location_http
        .post(&url)
        .json(&internal_req)
        .send()
        .await
        .map_err(|e| ErrorInternalServerError(format!("pushdown forward to {peer} failed: {e}")))?;
    let result: PushdownQueryResponse = resp
        .json()
        .await
        .map_err(|e| ErrorInternalServerError(format!("bad pushdown response from {peer}: {e}")))?;

    Ok(HttpResponse::Ok().json(result))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zlib_f64_codec_round_trips() {
        let values: Vec<f64> = vec![1.5, -2.0, 3.25, 100.0];
        let mut raw = Vec::new();
        for v in &values {
            raw.extend_from_slice(&v.to_le_bytes());
        }
        let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut encoder, &raw).unwrap();
        let compressed = encoder.finish().unwrap();

        let decoded = ZlibF64Codec.decode(&compressed).unwrap();
        assert_eq!(decoded, values);
    }

    #[test]
    fn cost_equation_matches_fusions_definition() {
        // Low selectivity, low compressibility -> push down.
        assert!(should_push_down_projection(0.01, 9.3));
        // High selectivity, high compressibility -> don't push down
        // (sending uncompressed values would cost more than the chunk).
        assert!(!should_push_down_projection(0.75, 152.0));
    }

    #[test]
    fn compressibility_is_uncompressed_over_compressed() {
        assert_eq!(compressibility(930, 100), 9.3);
        assert_eq!(compressibility(100, 0), 1.0); // guards div-by-zero, not a real case
    }
}
