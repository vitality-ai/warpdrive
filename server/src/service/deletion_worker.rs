//! Background deletion worker for processing deletion queue
//!
//! Request paths queue the extents of deleted/overwritten objects. Each pass
//! drains the queue: ranges no live object or in-progress upload still
//! references are erased on disk (hole punch), then the event is marked done.

use crate::service::metadata_service::MetadataService;
use crate::service::storage_service::StorageService;
use crate::service::user_context::UserContext;
use log::{info, warn, error, debug};
use std::collections::HashMap;
use std::time::Duration;
use tokio::time;

/// Background deletion worker
pub struct DeletionWorker {
    batch_size: i32,
    cleanup_interval: Duration,
}

impl DeletionWorker {
    pub fn new() -> Self {
        let secs = std::env::var("WARPDRIVE_DELETION_INTERVAL_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|s| *s > 0)
            .unwrap_or(300);
        Self {
            batch_size: 500,
            cleanup_interval: Duration::from_secs(secs),
        }
    }

    /// Start the deletion worker as a background task (non-blocking)
    pub fn start_background(self) -> tokio::task::JoinHandle<()> {
        info!("Starting deletion worker with {}s interval", self.cleanup_interval.as_secs());

        tokio::spawn(async move {
            let mut interval = time::interval(self.cleanup_interval);
            let batch_size = self.batch_size;

            loop {
                interval.tick().await;
                // SQLite + file I/O: keep it off the async runtime threads.
                match tokio::task::spawn_blocking(move || run_pass(batch_size)).await {
                    Ok(Ok(n)) if n > 0 => info!("Deletion pass reclaimed {} events", n),
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => error!("Error processing deletions: {}", e),
                    Err(e) => error!("Deletion pass panicked: {}", e),
                }
            }
        })
    }
}

/// Drain the deletion queue once. Returns how many events were fully reclaimed.
pub fn run_pass(batch_size: i32) -> Result<usize, String> {
    let metadata = MetadataService::new("system").map_err(|e| e.to_string())?;
    let storage = StorageService::new();
    let mut reclaimed = 0;
    let mut after_id = 0i64;

    loop {
        let events = metadata.get_pending_deletions(after_id, batch_size).map_err(|e| e.to_string())?;
        if events.is_empty() {
            break;
        }
        // Fresh per batch: every event in it was queued before this snapshot.
        let mut live: HashMap<(String, String), Vec<(u64, u64)>> = HashMap::new();

        for event in &events {
            after_id = after_id.max(event.id);
            let key = (event.user_id.clone(), event.bucket.clone());
            if !live.contains_key(&key) {
                match metadata.referenced_extents(&event.user_id, &event.bucket) {
                    Ok(mut v) => { v.sort_unstable(); live.insert(key.clone(), v); }
                    Err(e) => {
                        error!("Deletion {}: cannot load live extents for {}/{}: {}", event.id, event.user_id, event.bucket, e);
                        continue;
                    }
                }
            }
            let (free, still_used) = split_unreferenced(&event.offset_size_list, &live[&key]);

            let ctx = UserContext::with_bucket(event.user_id.clone(), event.bucket.clone());
            if !free.is_empty() {
                if let Err(e) = storage.delete_chunks(&ctx, &free) {
                    error!("Deletion {}: erase failed: {}", event.id, e);
                    continue;
                }
            }
            if still_used > 0 {
                // Still referenced (e.g. queued before its metadata row was removed): retry next pass.
                debug!("Deletion {}: {} ranges still referenced, keeping pending", event.id, still_used);
                continue;
            }
            if let Err(e) = metadata.mark_deletion_processed(event.id) {
                error!("Failed to mark deletion event {} as processed: {}", event.id, e);
                continue;
            }
            reclaimed += 1;
        }
    }

    if let Err(e) = metadata.cleanup_old_deletions() {
        warn!("Failed to cleanup old deletion events: {}", e);
    }
    Ok(reclaimed)
}

/// Split `ranges` into those overlapping no `live` extent (safe to erase) and a count of the rest.
/// `live` must be sorted.
fn split_unreferenced(ranges: &[(u64, u64)], live: &[(u64, u64)]) -> (Vec<(u64, u64)>, usize) {
    let mut free = Vec::with_capacity(ranges.len());
    let mut used = 0;
    for &(off, size) in ranges {
        let end = off.saturating_add(size);
        // First live extent starting at/after `end` can't overlap; check everything before it.
        let idx = live.partition_point(|&(lo, _)| lo < end);
        let overlaps = live[..idx].iter().rev().any(|&(lo, sz)| lo.saturating_add(sz) > off);
        if overlaps { used += 1 } else { free.push((off, size)) }
    }
    (free, used)
}

/// Start the deletion worker as a background task (non-blocking)
pub fn start_deletion_worker() -> tokio::task::JoinHandle<()> {
    let worker = DeletionWorker::new();
    worker.start_background()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_deletion_worker_creation() {
        let worker = DeletionWorker::new();
        assert_eq!(worker.batch_size, 500);
        assert_eq!(worker.cleanup_interval.as_secs(), 300);
    }

    #[test]
    fn split_keeps_ranges_a_live_object_still_uses() {
        let live = vec![(100, 50), (500, 10)];
        let (free, used) = split_unreferenced(&[(0, 100), (120, 10), (140, 20), (200, 300), (505, 1)], &live);
        assert_eq!(free, vec![(0, 100), (200, 300)]);
        assert_eq!(used, 3);
    }

    #[test]
    fn split_with_no_live_extents_frees_everything() {
        let (free, used) = split_unreferenced(&[(0, 10), (10, 10)], &[]);
        assert_eq!(free, vec![(0, 10), (10, 10)]);
        assert_eq!(used, 0);
    }
}
