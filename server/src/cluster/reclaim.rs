//! Which shards could be reclaimed (RFC 6, step 1: report only).
//!
//! Each node works out, on its own, which of the shards it holds nothing
//! references any more: those of deleted and overwritten objects, of
//! aborted multipart uploads, and of writes that never committed. It
//! decides from its own copy of the records, and asks its peers only when
//! that copy can't tell. This step frees nothing. It reports the count and
//! bytes, so the compactor (RFC 6 steps 3 and 4) has a checked liveness
//! rule to free by.
//!
//! **Which shards are referenced.** A shard is named by its shard key
//! `(user, bucket, wire_key, idx)`, and `wire_key` is `"{key}:{version}"`
//! where the version starts with the write time in nanoseconds. That gives
//! RFC 6's `written_at` without a new shard index field.
//!
//! | Shard | Referenced if | Unreferenced, past the grace period |
//! |---|---|---|
//! | S3 blob (bucket `\u{1}blobs\u{1}…`) | a live version or multipart-part row holds a blob with this `wire_key` | ask every peer |
//! | Location (plain key) | the key's live location record has this version | reclaimable if this node holds a newer record or tombstone for the key, else ask every peer |
//! | Packed stripe (`\0cdstripe:…`) | the key's live content record has this version | as location |
//! | Anything else | kept | kept |
//!
//! - **Grace period** (`WARPDRIVE_RECLAIM_GRACE_SECS`, default 600): a
//!   shard is written before the record that references it, so a shard
//!   younger than this is always kept.
//! - **Asking peers:** a candidate counts as reclaimable only if every
//!   peer answers that it doesn't reference it. A peer that doesn't answer
//!   means keep. A blob shard always takes this path, because a node that
//!   missed a row can't tell it from a deleted one.
//!
//! A sweep runs every `WARPDRIVE_RECLAIM_INTERVAL_SECS` (default 300) and
//! logs what it found. `WARPDRIVE_RECLAIM=off` turns it off.
//! `POST /cluster/_internal/reclaim/report` runs one now and returns it.

use std::collections::HashSet;
use std::time::Duration;

use actix_web::{web, HttpResponse};
use serde::{Deserialize, Serialize};
use warp_meta_store::Entry;

use super::blob_store::blob_bucket;
use super::coordinator::{self_addr, ClusterState};
use super::replicated_metadata::{rows, MetaValue};
use super::shard_storage::{parse_shard_key, shard_entries, ShardId};

/// Prefix of the internal bucket S3 blob shards are stored under.
const BLOB_NS: &str = "\u{1}blobs\u{1}";
/// Shards per confirmation request.
const CHECK_BATCH: usize = 1000;

#[derive(Debug, Clone, Copy)]
pub struct Config {
    pub enabled: bool,
    pub grace: Duration,
    pub interval: Duration,
}

fn env_secs(name: &str, default: u64) -> Duration {
    Duration::from_secs(std::env::var(name).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(default))
}

pub fn config() -> Config {
    Config {
        enabled: !matches!(std::env::var("WARPDRIVE_RECLAIM").as_deref().map(str::trim), Ok("off") | Ok("0") | Ok("false")),
        grace: env_secs("WARPDRIVE_RECLAIM_GRACE_SECS", 600),
        interval: env_secs("WARPDRIVE_RECLAIM_INTERVAL_SECS", 300),
    }
}

/// What one sweep found and did.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct SweepStats {
    pub shards: usize,
    pub kept_referenced: usize,
    pub kept_young: usize,
    pub kept_unrecognized: usize,
    /// No peer confirmed, or one didn't answer. Tried again next sweep.
    pub kept_unconfirmed: usize,
    /// This node held a newer record or tombstone for the key.
    pub reclaimable_superseded: usize,
    /// Every peer confirmed nothing references it.
    pub reclaimable_confirmed: usize,
    /// Sum of the reclaimable shards' sizes.
    pub bytes_reclaimable: u64,
}

/// The write time a shard's version carries: `"{nanos:x}-{seq:x}"`.
fn write_nanos(version: &str) -> Option<u64> {
    let hex = version.split('-').next()?;
    u64::from_str_radix(hex, 16).ok()
}

/// `stripe_key` inverted: `"\0cdstripe:{len}:{key}:{i}"` gives `(key, i)`.
fn parse_stripe_key(s: &str) -> Option<(String, usize)> {
    let rest = s.strip_prefix("\0cdstripe:")?;
    let (len, rest) = rest.split_once(':')?;
    let len: usize = len.parse().ok()?;
    let key = rest.get(..len)?;
    let idx = rest.get(len..)?.strip_prefix(':')?.parse().ok()?;
    Some((key.to_string(), idx))
}

/// Every `(user, shard bucket, wire_key)` a live S3 row references.
fn row_references() -> HashSet<(String, String, String)> {
    let Some(rows) = rows() else { return HashSet::new() };
    let mut refs = HashSet::new();
    let blobs = rows.list_live("", |_, r| match &r.value {
        MetaValue::Version(v) => v.blobs.clone(),
        MetaValue::Upload(u) => u.parts.values().flat_map(|p| p.blobs.clone()).collect(),
        _ => Vec::new(),
    });
    for b in blobs.into_iter().flatten() {
        if b.placement.inline.is_none() && !b.placement.wire_key.is_empty() {
            refs.insert((b.user_id.clone(), blob_bucket(&b.bucket), b.placement.wire_key.clone()));
        }
    }
    refs
}

/// What this node's records say about one shard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    Referenced,
    /// A newer record or tombstone for the key: the shard's version was
    /// overwritten or deleted.
    Superseded,
    /// Nothing here references it, and nothing here says why.
    NoEvidence,
    Unrecognized,
}

/// Reads a record entry's view of a shard at `version`, written at `nanos`.
fn entry_view<R: warp_meta_store::Record>(entry: Option<Entry<R>>, version: &str, nanos: u64) -> View {
    match entry {
        Some(e) if e.live().is_some_and(|r| r.version() == version) => View::Referenced,
        Some(e) if e.seq() > nanos => View::Superseded,
        _ => View::NoEvidence,
    }
}

fn view(state: &ClusterState, refs: &HashSet<(String, String, String)>, id: &ShardId) -> View {
    if id.bucket.starts_with(BLOB_NS) {
        return if refs.contains(&(id.user_id.clone(), id.bucket.clone(), id.wire_key.clone())) { View::Referenced } else { View::NoEvidence };
    }
    let Some((object, version)) = id.wire_key.rsplit_once(':') else { return View::Unrecognized };
    let Some(nanos) = write_nanos(version) else { return View::Unrecognized };
    if let Some((key, _)) = parse_stripe_key(object) {
        entry_view(state.content_location_store.entry(&id.user_id, &id.bucket, &key), version, nanos)
    } else {
        entry_view(state.location_store.entry(&id.user_id, &id.bucket, object), version, nanos)
    }
}

/// A shard in a confirmation request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShardRef {
    pub user_id: String,
    pub bucket: String,
    pub wire_key: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CheckRequest {
    pub shards: Vec<ShardRef>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CheckReply {
    /// One answer per requested shard, in order: does this node reference it.
    pub referenced: Vec<bool>,
}

/// Answers a peer's confirmation request from this node's records.
pub fn references(state: &ClusterState, shards: &[ShardRef]) -> Vec<bool> {
    let refs = row_references();
    shards
        .iter()
        .map(|s| {
            let id = ShardId { user_id: s.user_id.clone(), bucket: s.bucket.clone(), wire_key: s.wire_key.clone(), idx: 0 };
            // Anything this node can't read as unreferenced counts as referenced.
            !matches!(view(state, &refs, &id), View::Superseded | View::NoEvidence)
        })
        .collect()
}

pub async fn cluster_internal_reclaim_check(state: web::Data<ClusterState>, body: web::Json<CheckRequest>) -> HttpResponse {
    let state = state.into_inner();
    let shards = body.into_inner().shards;
    match web::block(move || references(&state, &shards)).await {
        Ok(referenced) => HttpResponse::Ok().json(CheckReply { referenced }),
        Err(e) => HttpResponse::InternalServerError().body(e.to_string()),
    }
}

pub async fn cluster_internal_reclaim_report(state: web::Data<ClusterState>) -> HttpResponse {
    let state = web::Data::clone(&state);
    match web::block(move || super::bridge::run(move || async move { sweep(&state).await })).await {
        Ok(stats) => HttpResponse::Ok().json(stats),
        Err(e) => HttpResponse::InternalServerError().body(e.to_string()),
    }
}

/// Asks every peer about `shards`. `true` where every peer answered that it
/// doesn't reference the shard. A peer that fails to answer keeps them all.
async fn confirmed_unreferenced(state: &ClusterState, shards: &[ShardRef]) -> Vec<bool> {
    let me = self_addr();
    let peers: Vec<String> = state.membership.peers().into_iter().filter(|p| Some(p.trim().trim_end_matches('/')) != me).collect();
    let mut free = vec![true; shards.len()];
    for (chunk_no, chunk) in shards.chunks(CHECK_BATCH).enumerate() {
        let base = chunk_no * CHECK_BATCH;
        for peer in &peers {
            let url = format!("{}/cluster/_internal/reclaim/check", peer.trim_end_matches('/'));
            let reply = state.location_http.post(&url).json(&CheckRequest { shards: chunk.to_vec() }).send().await;
            let answer = match reply {
                Ok(r) if r.status().is_success() => r.json::<CheckReply>().await.ok().filter(|a| a.referenced.len() == chunk.len()),
                _ => None,
            };
            match answer {
                Some(a) => {
                    for (i, referenced) in a.referenced.into_iter().enumerate() {
                        if referenced {
                            free[base + i] = false;
                        }
                    }
                }
                None => {
                    log::warn!("reclaim: no usable answer from {peer}, keeping {} candidates", chunk.len());
                    free[base..base + chunk.len()].iter_mut().for_each(|f| *f = false);
                }
            }
        }
    }
    free
}

fn now_nanos() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0)
}

/// One pass over this node's shards: what could be reclaimed now.
pub async fn sweep(state: &ClusterState) -> SweepStats {
    let cfg = config();
    let refs = row_references();
    let now = now_nanos();
    let grace = cfg.grace.as_nanos() as u64;
    let mut stats = SweepStats::default();
    let mut reclaimable: Vec<(String, ShardId, u64, u64)> = Vec::new();
    let mut ask: Vec<(String, ShardId, u64, u64)> = Vec::new();

    for (sk, offset, size) in shard_entries() {
        stats.shards += 1;
        let Some(id) = parse_shard_key(&sk) else {
            stats.kept_unrecognized += 1;
            continue;
        };
        let Some(nanos) = id.wire_key.rsplit_once(':').and_then(|(_, v)| write_nanos(v)) else {
            stats.kept_unrecognized += 1;
            continue;
        };
        if now.saturating_sub(nanos) < grace {
            stats.kept_young += 1;
            continue;
        }
        match view(state, &refs, &id) {
            View::Referenced => stats.kept_referenced += 1,
            View::Unrecognized => stats.kept_unrecognized += 1,
            View::Superseded => {
                stats.reclaimable_superseded += 1;
                reclaimable.push((sk, id, offset, size));
            }
            View::NoEvidence => ask.push((sk, id, offset, size)),
        }
    }

    if !ask.is_empty() {
        let shards: Vec<ShardRef> = ask.iter().map(|(_, id, _, _)| ShardRef { user_id: id.user_id.clone(), bucket: id.bucket.clone(), wire_key: id.wire_key.clone() }).collect();
        let verdicts = confirmed_unreferenced(state, &shards).await;
        for (candidate, ok) in ask.into_iter().zip(verdicts) {
            if ok {
                stats.reclaimable_confirmed += 1;
                reclaimable.push(candidate);
            } else {
                stats.kept_unconfirmed += 1;
            }
        }
    }

    stats.bytes_reclaimable = reclaimable.iter().map(|(_, _, _, size)| size).sum();
    if !reclaimable.is_empty() {
        log::info!("reclaim (report only): {stats:?}");
    }
    stats
}

/// Starts the background sweep on its own thread, if enabled.
pub fn start(state: web::Data<ClusterState>) {
    let cfg = config();
    if !cfg.enabled {
        log::info!("reclaim: off (WARPDRIVE_RECLAIM)");
        return;
    }
    log::info!("reclaim: sweeping every {:?}, grace {:?}", cfg.interval, cfg.grace);
    std::thread::Builder::new()
        .name("wd-reclaim".into())
        .spawn(move || loop {
            std::thread::sleep(cfg.interval);
            let s = web::Data::clone(&state);
            super::bridge::run(move || async move { sweep(&s).await });
        })
        .expect("failed to start the reclaim thread");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_write_time_from_a_version() {
        assert_eq!(write_nanos("18a2b-3f"), Some(0x18a2b));
        assert_eq!(write_nanos("zz-1"), None);
    }

    #[test]
    fn inverts_stripe_keys() {
        let sk = super::super::packed::stripe_key("a:b/c", 7);
        assert_eq!(parse_stripe_key(&sk), Some(("a:b/c".to_string(), 7)));
        assert_eq!(parse_stripe_key("plain"), None);
    }
}
