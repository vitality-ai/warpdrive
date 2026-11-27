//! Which shards can be reclaimed (RFC 6, step 1), and handing them to the
//! compactor (step 4).
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
//! A sweep runs every `WARPDRIVE_RECLAIM_INTERVAL_SECS` (default 300).
//! `WARPDRIVE_RECLAIM=off` turns it off.
//!
//! **Two sweeps before freeing.** A shard is handed to the compactor only
//! if it was reclaimable in two sweeps at least
//! `WARPDRIVE_RECLAIM_CONFIRM_SECS` apart (default 60). A shard stays
//! unreferenced once it is, so it has been dead for that long: a GET that
//! read a version's record just before an overwrite has finished with its
//! shards by then.
//!
//! - `POST /cluster/_internal/reclaim/report` sweeps now and reports, frees
//!   nothing.
//! - `POST /cluster/_internal/reclaim/run` sweeps and runs a compaction
//!   pass now, as the background thread does.

use std::collections::HashSet;
use std::time::Duration;

use actix_web::{web, HttpResponse};
use serde::{Deserialize, Serialize};
use warp_meta_store::Entry;

use super::blob_store::blob_bucket;
use super::coordinator::{self_addr, ClusterState};
use super::shard_storage::{parse_shard_key, shard_entries, ShardId};

/// Prefix of the internal bucket S3 blob shards are stored under.
const BLOB_NS: &str = "\u{1}blobs\u{1}";
/// Shards per confirmation request. A peer reads all of its records once
/// per request to answer, so few large requests: about 2.5 MB of JSON
/// each, against hundreds of thousands of candidates on a busy node.
const CHECK_BATCH: usize = 20_000;

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
        // A daily audit: `gc/` rows free what overwrites and deletes kill
        // (gc.rs). The sweep finds what no row names.
        interval: env_secs("WARPDRIVE_RECLAIM_INTERVAL_SECS", 86_400),
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

/// Which blobs the rows reference. The layer that writes the rows knows
/// what they mean, so it answers. The audit frees shards no row needs.
pub trait BlobReferences: Send + Sync {
    /// Every blob a live row references. The scan isn't a snapshot, so an
    /// implementation whose rows move references orders its scan so that a
    /// moving reference is always seen.
    fn referenced(&self, rows: &super::rows::NodeRows) -> Vec<super::blob_store::BlobRef>;
}

/// Every `(user, shard bucket, wire_key)` a live row references.
fn row_references(state: &ClusterState) -> HashSet<(String, String, String)> {
    let (Some(rows), Some(refs_of)) = (state.rows.get(), state.blob_references.get()) else { return HashSet::new() };
    let mut refs = HashSet::new();
    for b in refs_of.referenced(rows) {
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
    let refs = row_references(state);
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
    sweep_detail(state).await.0
}

/// A shard the sweep found reclaimable: (shard key, offset, size).
pub type Dead = (String, u64, u64);

/// `sweep`, with the reclaimable shards themselves.
pub async fn sweep_detail(state: &ClusterState) -> (SweepStats, Vec<Dead>) {
    let cfg = config();
    let refs = row_references(state);
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
        log::info!("reclaim: {stats:?}");
    }
    (stats, reclaimable.into_iter().map(|(sk, _, offset, size)| (sk, offset, size)).collect())
}

/// The last sweep that counts as the first of two: when, and what it found.
static FIRST_SWEEP: parking_lot::Mutex<Option<(std::time::Instant, HashSet<Dead>)>> = parking_lot::Mutex::new(None);

/// The shards reclaimable now that were also reclaimable in a sweep at
/// least `gap` ago. Records `now_dead` as the next first sweep once the
/// gap has passed.
fn confirmed(now_dead: Vec<Dead>, gap: Duration) -> Vec<Dead> {
    let mut first = FIRST_SWEEP.lock();
    let out = match first.as_ref() {
        Some((at, seen)) if at.elapsed() >= gap => now_dead.iter().filter(|d| seen.contains(*d)).cloned().collect(),
        Some(_) => return Vec::new(),
        None => Vec::new(),
    };
    *first = Some((std::time::Instant::now(), now_dead.into_iter().collect()));
    out
}

/// What one reclaim run found and freed.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunStats {
    pub sweep: SweepStats,
    /// Reclaimable in this sweep and in one at least the confirm gap ago.
    pub confirmed_dead: usize,
    pub compact: super::compactor::CompactStats,
}

/// A sweep, then a compaction pass on what two sweeps agree is dead.
/// Blocks: call it off the async workers.
pub fn run(state: web::Data<ClusterState>) -> RunStats {
    let (sweep, dead) = super::bridge::run(move || async move { sweep_detail(&state).await });
    let dead = confirmed(dead, env_secs("WARPDRIVE_RECLAIM_CONFIRM_SECS", 60));
    let compact = super::compactor::run_pass(&dead);
    if compact != Default::default() {
        log::info!("compactor: {compact:?}");
    }
    RunStats { sweep, confirmed_dead: dead.len(), compact }
}

pub async fn cluster_internal_reclaim_run(state: web::Data<ClusterState>) -> HttpResponse {
    match web::block(move || run(state)).await {
        Ok(stats) => HttpResponse::Ok().json(stats),
        Err(e) => HttpResponse::InternalServerError().body(e.to_string()),
    }
}

/// How often the record logs are checked for compaction (RFC 6 step 2).
const LOG_CHECK: Duration = Duration::from_secs(60);

/// Compacts every record log that has built up enough dead lines: the S3
/// metadata rows, the shard index, and the location and content-location
/// logs. Writers aren't stalled while one runs. Returns the logs compacted.
pub fn compact_due_logs(state: &ClusterState) -> Vec<&'static str> {
    let mut done = Vec::new();
    let mut run = |name: &'static str, due: bool, compact: &dyn Fn() -> std::io::Result<()>| {
        if due {
            match compact() {
                Ok(()) => done.push(name),
                Err(e) => log::warn!("compaction of the {name} log failed: {e}"),
            }
        }
    };
    if let Some(rows) = state.rows.get() {
        use warp_meta_store::Engine;
        run("rows", rows.log().engine().compaction_due(), &|| rows.log().engine().compact());
    }
    run("shard index", super::shard_storage::shard_index_compaction_due(), &super::shard_storage::compact_shard_index);
    run("location", state.location_store.compaction_due(), &|| state.location_store.compact());
    run("content location", state.content_location_store.compaction_due(), &|| state.content_location_store.compact());
    done
}

/// Starts the background thread: record-log compaction every minute, and
/// the reclaimable-shard sweep every `WARPDRIVE_RECLAIM_INTERVAL_SECS`.
/// `WARPDRIVE_RECLAIM=off` turns off the sweep. Log compaction always runs.
pub fn start(state: web::Data<ClusterState>) {
    let cfg = config();
    log::info!("record logs: compacted when due, checked every {LOG_CHECK:?}");
    if cfg.enabled {
        log::info!("reclaim: sweeping every {:?}, grace {:?}, compactor {:?}", cfg.interval, cfg.grace, super::compactor::config());
    } else {
        log::info!("reclaim: sweep off (WARPDRIVE_RECLAIM)");
    }
    std::thread::Builder::new()
        .name("wd-maintenance".into())
        .spawn(move || {
            let mut since_sweep = Duration::ZERO;
            loop {
                std::thread::sleep(LOG_CHECK);
                let compacted = compact_due_logs(&state);
                if !compacted.is_empty() {
                    log::info!("compacted record logs: {}", compacted.join(", "));
                }
                let gc = super::gc::run(&state);
                if gc.rows > 0 || gc.compact != Default::default() {
                    log::info!("gc: {gc:?}");
                }
                since_sweep += LOG_CHECK;
                if cfg.enabled && since_sweep >= cfg.interval {
                    since_sweep = Duration::ZERO;
                    run(web::Data::clone(&state));
                }
            }
        })
        .expect("failed to start the maintenance thread");
}

pub async fn cluster_internal_compact_logs(state: web::Data<ClusterState>) -> HttpResponse {
    let state = state.into_inner();
    match web::block(move || compact_due_logs(&state)).await {
        Ok(done) => HttpResponse::Ok().json(done),
        Err(e) => HttpResponse::InternalServerError().body(e.to_string()),
    }
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
