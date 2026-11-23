//! Freeing dead shards from the blobs writes record as dead (RFC 6, step 6).
//!
//! When an overwrite, delete, part re-upload or abort drops a blob, the
//! coordinator writes a `gc/{hlc}` row naming it, replicated like any row
//! and written after the rows that dropped it. So a crash in between leaks
//! the blob's space and never frees a referenced one.
//!
//! Every minute each node takes the `gc/` rows past the grace period
//! (`WARPDRIVE_RECLAIM_GRACE_SECS`) that it hasn't processed yet, names its
//! own shards of those blobs, and hands them to the compactor, which drops
//! their index entries and frees their space. A local cursor (`gc_cursor`)
//! remembers how far it got, so a row is processed once, and a processed
//! row leaves this node's copy at once: each node keeps its copy only for
//! its own shards. A pass reads only the rows after the cursor and past the
//! grace period, a range of time-ordered keys, so it costs what died since
//! the last pass and not what is stored.
//!
//! The work is proportional to what died, with no scan and no questions to
//! peers. The reclaim sweep (`reclaim.rs`) stays as a daily audit for what
//! no `gc/` row names: shards of writes that never committed, and rows a
//! node missed.


use serde::{Deserialize, Serialize};
use warp_meta_store::hlc;

use super::blob_store::{blob_bucket, BlobRef};
use super::replicated_metadata::{rows, MetaValue};
use super::shard_storage::{shard_key, shard_location};

const PREFIX: &str = "gc/";

/// A new `gc/` row key: ordered by time.
pub fn new_key() -> String {
    format!("{PREFIX}{:016x}", hlc::now())
}

fn key_nanos(key: &str) -> Option<u64> {
    u64::from_str_radix(key.strip_prefix(PREFIX)?, 16).ok()
}

/// The blobs a gc/ row names: every dead blob, inline ones included. An inline
/// blob has no shards to free, but its address index entry goes too.
pub fn with_shards(blobs: Vec<BlobRef>) -> Vec<BlobRef> {
    blobs
}

fn cursor_path() -> String {
    std::env::var("WARPDRIVE_GC_CURSOR").unwrap_or_else(|_| "gc_cursor".to_string())
}

fn load_cursor() -> String {
    std::fs::read_to_string(cursor_path()).map(|s| s.trim().to_string()).unwrap_or_default()
}

/// Durable before it returns: written to a temp file, synced, renamed.
fn save_cursor(key: &str) -> std::io::Result<()> {
    use std::io::Write;
    let path = cursor_path();
    let tmp = format!("{path}.tmp");
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(key.as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, &path)
}

/// What one pass found and freed.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct GcStats {
    /// `gc/` rows past the grace period and not processed before.
    pub rows: usize,
    pub blobs: usize,
    /// Shards of those blobs this node holds.
    pub shards: usize,
    pub rows_retired: usize,
    pub compact: super::compactor::CompactStats,
}

/// One pass: frees this node's shards of the blobs in `gc/` rows past the
/// grace period, then runs the compactor. Blocks: call it off the async
/// workers.
pub fn run() -> GcStats {
    // One pass at a time: the cursor is read, then written.
    static ONE: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
    let _one = ONE.lock();
    let mut stats = GcStats::default();
    // With the compactor off nothing is freed, so the cursor must not move.
    if !super::compactor::config().enabled {
        return stats;
    }
    let Some(rows) = rows() else { return stats };
    let grace = super::reclaim::config().grace.as_nanos() as u64;
    let now = hlc::now();
    let cursor = load_cursor();
    // Keys are gc/{hlc}: the rows due are the range after the cursor up to
    // now minus the grace period.
    let upto = format!("{PREFIX}{:016x}", now.saturating_sub(grace));
    let after = if cursor.is_empty() { PREFIX.to_string() } else { cursor.clone() };
    let due: Vec<(String, Vec<BlobRef>)> = rows
        .list_live_range(Some(&after), &upto, |k, r| match &r.value {
            MetaValue::Gc(blobs) if k.starts_with(PREFIX) => Some((k.to_string(), blobs.clone())),
            _ => None,
        })
        .into_iter()
        .flatten()
        .collect();
    let mut dead = Vec::new();
    for (_, blobs) in &due {
        for b in blobs {
            stats.blobs += 1;
            // Past the grace period no read is on it: out of the address
            // index, which otherwise grows with every write ever made.
            super::blob_store::unindex_blob(b);
            if b.placement.inline.is_some() || b.placement.wire_key.is_empty() {
                continue;
            }
            let bucket = blob_bucket(&b.bucket);
            for idx in 0..b.placement.shard_peers.len() {
                let sk = shard_key(&b.user_id, &bucket, &b.placement.wire_key, idx);
                if let Some((offset, size)) = shard_location(&sk) {
                    dead.push((sk, offset, size));
                }
            }
        }
    }
    stats.rows = due.len();
    stats.shards = dead.len();
    stats.compact = super::compactor::run_pass(&dead);
    // After the compactor dropped the entries: a crash before this line
    // processes the rows again, which finds nothing left to drop.
    if let Some((last, _)) = due.last() {
        if let Err(e) = save_cursor(last) {
            log::warn!("gc: saving the cursor failed: {e}");
            return stats;
        }
        // Processed: out of this node's copy, in one batch with one sync. No
        // tombstone: a gc/ key is written once, and its replicas land within
        // milliseconds, long before it is due.
        use warp_meta_store::Engine;
        let keys: Vec<String> = due.iter().map(|(k, _)| k.clone()).collect();
        match rows.engine().delete_many_if(&keys, |_, _| false) {
            Ok(n) => stats.rows_retired = n,
            Err(e) => log::warn!("gc: dropping processed rows failed: {e}"),
        }
    }
    stats
}

pub async fn cluster_internal_gc_run() -> actix_web::HttpResponse {
    match actix_web::web::block(run).await {
        Ok(stats) => actix_web::HttpResponse::Ok().json(stats),
        Err(e) => actix_web::HttpResponse::InternalServerError().body(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_order_by_time_and_read_back_their_time() {
        let (a, b) = (new_key(), new_key());
        assert!(a < b);
        assert!(key_nanos(&a).unwrap() < key_nanos(&b).unwrap());
        assert_eq!(key_nanos("gc/zz"), None);
    }
}
