//! LocalXFSSlabStore — co-locating slab layout within a single per-bucket binary file.
//!
//! The file is divided into fixed-size slot windows (SLAB_WINDOW bytes each).  Writes
//! that carry the same `slab_hint` are routed to the same slot so that related objects
//! (e.g. all delta layers for one Neon timeline checkpoint) land in one contiguous
//! region.  Reads and the on-disk format are identical to the flat store — the offset
//! returned to SQLite is the real byte position in the file — so the two backends can
//! be swapped without touching metadata.
//!
//! Concurrency: slot allocation is serialised by SLOT_INDEX mutex; the actual file I/O
//! uses pwrite/pread (FileExt::write_at / read_at) which are safe for non-overlapping
//! concurrent access without a global write lock.

use crate::metadata::sqlite_store::SQLiteMetadataStore;
use crate::storage::Storage;
use actix_web::Error;
use actix_web::error::ErrorInternalServerError;
use lazy_static::lazy_static;
use log::debug;
use std::collections::HashMap;
use std::env;
use std::fs::OpenOptions;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::Mutex;

// ── configuration ────────────────────────────────────────────────────────────

/// Default slab window: 4 MB (matches Neon checkpoint_distance for the baseline
/// experiment).  For a Neon workload with k deltas of average size d, set
/// SLAB_WINDOW = k * d so all deltas for a page fit in one slot.
const DEFAULT_SLAB_WINDOW: u64 = 128 * 1024 * 1024;

fn slab_window() -> u64 {
    env::var("SLAB_WINDOW")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_SLAB_WINDOW)
}

fn storage_dir() -> PathBuf {
    let p = env::var("STORAGE_DIRECTORY")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("storage"));
    std::fs::create_dir_all(&p).expect("create storage dir");
    p
}

// ── per-bucket slot state ────────────────────────────────────────────────────

struct BucketSlots {
    /// hint → (current slot id, bytes used in that slot)
    active: HashMap<String, (u64, u64)>,
    /// next slot id to hand out
    next_slot: u64,
}

lazy_static! {
    /// Global slot index keyed by "user_id/bucket".
    static ref SLOT_INDEX: Mutex<HashMap<String, BucketSlots>> = Mutex::new(HashMap::new());
}

// ── store ────────────────────────────────────────────────────────────────────

pub struct LocalXFSSlabStore;

impl LocalXFSSlabStore {
    pub fn new() -> Self {
        Self
    }

    fn bucket_path(&self, user_id: &str, bucket: &str) -> PathBuf {
        let dir = storage_dir().join(user_id);
        std::fs::create_dir_all(&dir).expect("create user dir");
        dir.join(format!("{}.bin", bucket))
    }

    /// Derive `next_slot` from the current file size so restarts never overwrite
    /// data written in a previous run.
    fn next_slot_from_disk(&self, user_id: &str, bucket: &str, window: u64) -> u64 {
        let path = self.bucket_path(user_id, bucket);
        std::fs::metadata(&path)
            .map(|m| {
                let sz = m.len();
                if sz == 0 { 0 } else { (sz + window - 1) / window }
            })
            .unwrap_or(0)
    }

    /// Warm the in-memory slot map from the metadata DB so restarts resume
    /// filling partially-used slots rather than always starting a new one.
    fn warm_from_db(slots: &mut BucketSlots, user_id: &str, bucket: &str, window: u64) {
        let db = SQLiteMetadataStore::new();
        // For each hint, find the highest offset written so far. If that slot
        // still has room, resume filling it; otherwise we leave active empty and
        // a fresh slot will be opened on the next write.
        let per_hint = match db.max_offset_per_hint(user_id, bucket) {
            Ok(v) => v,
            Err(_) => return,
        };
        for (hint, max_off) in per_hint {
            let slot_id = max_off / window;
            // Scan all objects with this hint in this slot to find how many bytes
            // are already used so we know where to write next.
            let used = match db.bytes_used_in_slot(user_id, bucket, &hint, slot_id, window) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if used < window {
                slots.active.insert(hint, (slot_id, used));
            }
        }
    }

    /// Reserve `data_len` bytes for `hint` and return the file offset to write at.
    fn allocate(&self, user_id: &str, bucket: &str, hint: &str, data_len: u64) -> u64 {
        let window = slab_window();
        let key = format!("{}/{}", user_id, bucket);

        let init_next = self.next_slot_from_disk(user_id, bucket, window);

        let mut idx = SLOT_INDEX.lock().unwrap();
        let is_new = !idx.contains_key(&key);
        let slots = idx.entry(key).or_insert_with(|| BucketSlots {
            active: HashMap::new(),
            next_slot: init_next,
        });

        // On first access after startup, rebuild active slot map from the DB
        // so we resume partially-filled slots instead of always opening new ones.
        if is_new {
            Self::warm_from_db(slots, user_id, bucket, window);
        }

        if let Some((slot_id, used)) = slots.active.get_mut(hint) {
            if *used + data_len <= window {
                let offset = *slot_id * window + *used;
                *used += data_len;
                return offset;
            }
        }

        // Current slot for this hint is full (or doesn't exist yet) — open a new one.
        let new_slot = slots.next_slot;
        slots.next_slot += 1;
        slots.active.insert(hint.to_string(), (new_slot, data_len));
        new_slot * window
    }
}

impl Storage for LocalXFSSlabStore {
    fn write(
        &self,
        user_id: &str,
        bucket: &str,
        data: &[u8],
        slab_hint: Option<&str>,
    ) -> Result<(u64, u64), Error> {
        let hint = slab_hint.unwrap_or("__default__");
        let data_len = data.len() as u64;
        let offset = self.allocate(user_id, bucket, hint, data_len);

        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(self.bucket_path(user_id, bucket))
            .map_err(ErrorInternalServerError)?;

        file.write_at(data, offset)
            .map_err(ErrorInternalServerError)?;

        debug!(
            "SlabStore write: hint={} offset={} size={} (slot={})",
            hint,
            offset,
            data_len,
            offset / slab_window()
        );
        Ok((offset, data_len))
    }

    fn read(
        &self,
        user_id: &str,
        bucket: &str,
        offset: u64,
        size: u64,
    ) -> Result<Vec<u8>, Error> {
        let file = OpenOptions::new()
            .read(true)
            .open(self.bucket_path(user_id, bucket))
            .map_err(ErrorInternalServerError)?;

        let mut buf = vec![0u8; size as usize];
        file.read_at(&mut buf, offset)
            .map_err(ErrorInternalServerError)?;
        Ok(buf)
    }

    fn delete(
        &self,
        user_id: &str,
        bucket: &str,
        offset_size_list: &[(u64, u64)],
    ) -> Result<(), Error> {
        use crate::metadata::sqlite_store::SQLiteMetadataStore;
        SQLiteMetadataStore::new().queue_deletion(user_id, bucket, "", offset_size_list)?;
        debug!(
            "SlabStore delete: queued {} ranges for {}/{}",
            offset_size_list.len(),
            user_id,
            bucket
        );
        Ok(())
    }

    fn verify(
        &self,
        user_id: &str,
        bucket: &str,
        offset: u64,
        size: u64,
        checksum: &[u8],
    ) -> Result<bool, Error> {
        use sha2::{Digest, Sha256};
        let data = self.read(user_id, bucket, offset, size)?;
        let mut h = Sha256::new();
        h.update(&data);
        Ok(h.finalize().as_slice() == checksum)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_same_hint_lands_in_same_slot() {
        let store = LocalXFSSlabStore::new();
        let (u, b) = ("test_slab_user", "test_slab_bucket");

        let (off1, _) = store.write(u, b, b"hello", Some("hint-A")).unwrap();
        let (off2, _) = store.write(u, b, b"world", Some("hint-A")).unwrap();
        let (off3, _) = store.write(u, b, b"other", Some("hint-B")).unwrap();

        let window = slab_window();
        assert_eq!(off1 / window, off2 / window, "same hint must share a slot");
        assert_ne!(off1 / window, off3 / window, "different hints must use different slots");

        let d1 = store.read(u, b, off1, 5).unwrap();
        let d2 = store.read(u, b, off2, 5).unwrap();
        assert_eq!(d1, b"hello");
        assert_eq!(d2, b"world");
    }

    #[test]
    fn test_slot_overflow_starts_new_slot() {
        // Use a tiny window so we can trigger overflow easily.
        // We can't override SLAB_WINDOW in tests, so just verify the allocator
        // logic by calling allocate directly with a small window.
        let store = LocalXFSSlabStore::new();
        let (u, b) = ("test_slab_overflow_user", "test_slab_overflow_bucket");
        let window = slab_window();

        // Write one chunk that fills the whole window
        let big = vec![0u8; window as usize];
        let (off1, _) = store.write(u, b, &big, Some("overflow-hint")).unwrap();

        // Next write for the same hint must spill to a new slot
        let (off2, _) = store.write(u, b, b"x", Some("overflow-hint")).unwrap();
        assert_ne!(off1 / window, off2 / window, "overflow must start a new slot");
    }
}
