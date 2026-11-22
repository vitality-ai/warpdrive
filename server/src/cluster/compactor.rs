//! Frees the space of dead shards (RFC 6, step 4).
//!
//! Each node runs this on its own, after a reclaim sweep (`reclaim.rs`) has
//! said which of its shards nothing references any more:
//!
//! 1. **Drop dead entries.** A dead shard's index entry is removed, only if
//!    it still points where the sweep saw it.
//! 2. **Measure segments.** For each bucket, live bytes per sealed segment
//!    come from the shard index, taken after the segment list. A segment
//!    counts as sealed once it stopped taking writes and nothing has written
//!    to it for `WARPDRIVE_COMPACT_SEAL_SECS` (default: the reclaim grace,
//!    600 s), so no write reserved before the roll is still in flight.
//! 3. **Pick by cost-benefit.** A segment whose live fraction `u` is under
//!    `WARPDRIVE_COMPACT_UMAX` (default 0.65) is a candidate, ranked by
//!    `(1 - u) * age / (1 + u)`: the most free space for the copying, with
//!    old cold data first.
//! 4. **Copy survivors** to the survivor head, at most
//!    `WARPDRIVE_COMPACT_MBPS` (default 50). Each copy is durable before its
//!    index entry is moved, and the move happens only if the entry still
//!    points at the old copy.
//! 5. **Delete the segment** once nothing points into it, after
//!    `WARPDRIVE_COMPACT_UNLINK_SECS` (default 30), so a read that looked up
//!    the old offset just before the move still finds the file.
//!
//! A crash at any point leaves every live shard readable: the old segment
//! stays until its entries have moved, and a copy whose entry never moved
//! is dead bytes in a survivor segment, freed by a later pass.
//! `WARPDRIVE_COMPACT=off` turns the compactor off; sweeps still report.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use super::shard_storage::{delete_shard_if, move_shard_if, parse_shard_key, shard_entries};
use crate::storage::local_store::{get_storage_directory, segment_of, LocalXFSBinaryStore};
use crate::storage::Storage;

#[derive(Debug, Clone, Copy)]
pub struct Config {
    pub enabled: bool,
    pub umax: f64,
    pub bytes_per_sec: u64,
    pub seal_idle: Duration,
    pub unlink_grace: Duration,
}

pub fn config() -> Config {
    let f = |name: &str, default: f64| std::env::var(name).ok().and_then(|v| v.trim().parse::<f64>().ok()).filter(|v| *v >= 0.0).unwrap_or(default);
    Config {
        enabled: !matches!(std::env::var("WARPDRIVE_COMPACT").as_deref().map(str::trim), Ok("off") | Ok("0") | Ok("false")),
        umax: f("WARPDRIVE_COMPACT_UMAX", 0.65).min(1.0),
        bytes_per_sec: (f("WARPDRIVE_COMPACT_MBPS", 50.0) * 1_048_576.0) as u64,
        seal_idle: Duration::from_secs_f64(f("WARPDRIVE_COMPACT_SEAL_SECS", super::reclaim::config().grace.as_secs_f64())),
        unlink_grace: Duration::from_secs_f64(f("WARPDRIVE_COMPACT_UNLINK_SECS", 30.0)),
    }
}

/// What one compaction pass did.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct CompactStats {
    pub dead_entries_dropped: usize,
    pub segments_sealed: usize,
    pub segments_compacted: usize,
    pub segments_deleted: usize,
    pub shards_moved: usize,
    /// The entry changed between the snapshot and the move: left alone.
    pub moves_skipped: usize,
    pub bytes_copied: u64,
    pub bytes_freed: u64,
    /// Shard keys that don't parse: their bucket is unknown, so no segment
    /// is compacted while any exist.
    pub unrecognized: usize,
}

struct Pending {
    user: String,
    bucket: String,
    seg: u64,
    len: u64,
    since: Instant,
}

static PENDING: LazyLock<Mutex<Vec<Pending>>> = LazyLock::new(|| Mutex::new(Vec::new()));
/// A pass starts no new segment after this long.
const PASS_MAX: Duration = Duration::from_secs(300);
/// One pass at a time.
static RUNNING: Mutex<()> = Mutex::new(());

/// Live index entries per (user, bucket), then per segment.
type Live = HashMap<(String, String), HashMap<u64, Vec<(String, u64, u64)>>>;

fn live_entries(stats: &mut CompactStats) -> Live {
    let mut live: Live = HashMap::new();
    for (sk, offset, size) in shard_entries() {
        match parse_shard_key(&sk) {
            Some(id) => live.entry((id.user_id, id.bucket)).or_default().entry(segment_of(offset)).or_default().push((sk, offset, size)),
            None => stats.unrecognized += 1,
        }
    }
    live
}

/// Every (user, bucket) directory under the storage root.
fn bucket_dirs() -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Ok(users) = std::fs::read_dir(get_storage_directory()) else { return out };
    for u in users.flatten().filter(|e| e.path().is_dir()) {
        let Some(user) = u.file_name().to_str().map(str::to_string) else { continue };
        let Ok(buckets) = std::fs::read_dir(u.path()) else { continue };
        for b in buckets.flatten().filter(|e| e.path().is_dir()) {
            if let Some(bucket) = b.file_name().to_str() {
                out.push((user.clone(), bucket.to_string()));
            }
        }
    }
    out
}

/// Deletes the segments whose grace has passed, if nothing points into
/// them. Returns how many it deleted and the bytes freed.
fn retire_due(store: &LocalXFSBinaryStore, cfg: &Config, stats: &mut CompactStats) {
    let due: Vec<Pending> = {
        let mut p = PENDING.lock();
        let (due, wait) = std::mem::take(&mut *p).into_iter().partition(|x| x.since.elapsed() >= cfg.unlink_grace);
        *p = wait;
        due
    };
    if due.is_empty() {
        return;
    }
    // Checked against a fresh snapshot: a segment someone still points
    // into is never deleted.
    let live = live_entries(&mut CompactStats::default());
    for p in due {
        let still = live.get(&(p.user.clone(), p.bucket.clone())).and_then(|m| m.get(&p.seg)).map_or(0, Vec::len);
        if still > 0 {
            log::warn!("compactor: segment {} of {:?} still has {still} entries, kept", p.seg, p.bucket);
            continue;
        }
        match store.retire(&p.user, &p.bucket, p.seg) {
            Ok(()) => {
                stats.segments_deleted += 1;
                stats.bytes_freed += p.len;
            }
            Err(e) => log::warn!("compactor: deleting segment {} of {:?} failed: {e}", p.seg, p.bucket),
        }
    }
}

/// One pass: drops the index entries of `dead` shards (key, offset, size),
/// compacts sealed segments under `umax`, and deletes segments that are
/// empty and past their grace.
pub fn run_pass(dead: &[(String, u64, u64)]) -> CompactStats {
    let cfg = config();
    let mut stats = CompactStats::default();
    if !cfg.enabled {
        return stats;
    }
    let _one = RUNNING.lock();
    let store = LocalXFSBinaryStore::new();
    retire_due(&store, &cfg, &mut stats);

    for (sk, offset, size) in dead {
        match delete_shard_if(sk, (*offset, *size)) {
            Ok(true) => stats.dead_entries_dropped += 1,
            Ok(false) => {}
            Err(e) => log::warn!("compactor: dropping the entry of {sk:?} failed: {e}"),
        }
    }

    // Segments first, then the index: a write that finished after the
    // listing is in the index, so it's counted as live.
    let mut sealed = Vec::new();
    for (user, bucket) in bucket_dirs() {
        match store.segments(&user, &bucket) {
            Ok(segs) => sealed.extend(segs.into_iter().filter(|s| !s.active && s.idle >= cfg.seal_idle).map(|s| (user.clone(), bucket.clone(), s))),
            Err(e) => log::warn!("compactor: listing segments of {bucket:?} failed: {e}"),
        }
    }
    let mut live = live_entries(&mut stats);
    if stats.unrecognized > 0 {
        log::warn!("compactor: {} shard keys don't parse, no segment compacted", stats.unrecognized);
        return stats;
    }
    stats.segments_sealed = sealed.len();
    let already: Vec<(String, String, u64)> = PENDING.lock().iter().map(|p| (p.user.clone(), p.bucket.clone(), p.seg)).collect();

    let mut candidates = Vec::new();
    for (user, bucket, seg) in sealed {
        if already.iter().any(|(u, b, s)| *u == user && *b == bucket && *s == seg.id) {
            continue;
        }
        let entries = live.get_mut(&(user.clone(), bucket.clone())).and_then(|m| m.remove(&seg.id)).unwrap_or_default();
        let live_bytes: u64 = entries.iter().map(|(_, _, n)| n).sum();
        if entries.is_empty() {
            PENDING.lock().push(Pending { user, bucket, seg: seg.id, len: seg.len, since: Instant::now() });
            continue;
        }
        let u = if seg.len == 0 { 1.0 } else { live_bytes as f64 / seg.len as f64 };
        if u < cfg.umax {
            let score = (1.0 - u) * seg.idle.as_secs_f64() / (1.0 + u);
            candidates.push((score, user, bucket, seg, entries));
        }
    }
    candidates.sort_by(|a, b| b.0.total_cmp(&a.0));

    let started = Instant::now();
    for (_, user, bucket, seg, mut entries) in candidates {
        // Bounded, so the maintenance thread gets back to the record logs.
        // What's left is picked up next pass.
        if started.elapsed() > PASS_MAX {
            break;
        }
        entries.sort_by_key(|(_, o, _)| *o);
        let mut failed = false;
        for (sk, offset, size) in entries {
            let moved = store
                .read(&user, &bucket, offset, size)
                .and_then(|bytes| store.write_survivor(&user, &bucket, &bytes))
                .map_err(|e| e.to_string())
                .and_then(|to| move_shard_if(&sk, (offset, size), to).map_err(|e| e.to_string()));
            match moved {
                Ok(true) => stats.shards_moved += 1,
                Ok(false) => stats.moves_skipped += 1,
                Err(e) => {
                    log::warn!("compactor: moving {sk:?} out of segment {} failed: {e}", seg.id);
                    failed = true;
                    break;
                }
            }
            stats.bytes_copied += size;
            // Rate limit: never ahead of bytes_per_sec on average.
            if cfg.bytes_per_sec > 0 {
                let due = Duration::from_secs_f64(stats.bytes_copied as f64 / cfg.bytes_per_sec as f64);
                if let Some(wait) = due.checked_sub(started.elapsed()) {
                    std::thread::sleep(wait);
                }
            }
        }
        if !failed {
            stats.segments_compacted += 1;
            PENDING.lock().push(Pending { user, bucket, seg: seg.id, len: seg.len, since: Instant::now() });
        }
    }
    retire_due(&store, &cfg, &mut stats);
    stats
}
