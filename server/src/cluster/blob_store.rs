//! `Storage` on the cluster: where the S3 handlers keep object bytes when
//! this node is part of a cluster.
//!
//! The handlers treat bytes as `(offset, size)` ranges in a bucket and do
//! arithmetic on them (a Range GET or a copy range slices a stored range).
//! Single-node mode backs that with one file per bucket. Here, each write
//! reserves a range of a per-bucket address space and is stored as one
//! erasure-coded blob. A read finds the blob whose range holds the address
//! and slices it.
//!
//! **No record per blob.** A blob's placement travels in the S3 metadata
//! row of the object that uses it, so a PUT replicates one record. Every
//! node keeps an in-memory index from address to placement: the writing
//! node adds a blob when it writes it, and every node adds the blobs of
//! each object or upload row it applies, and of every row at startup.
//!
//! **Addresses are unique without coordination.** The top 8 bits are this
//! node's slot (its position in `WARPDRIVE_PEERS`, or
//! `WARPDRIVE_NODE_SLOT`), and the low 56 bits come from a per-bucket
//! counter on this node. The counter reserves ahead in 1 GiB steps and
//! makes each reservation durable before handing out addresses from it, so
//! a restart skips forward and never reuses an address.
//!
//! **Freeing space:** `delete` frees nothing yet. A blob can be referenced
//! by more than one object (a completed multipart upload reuses its parts'
//! ranges), so freeing one needs a reference check. That comes with space
//! reclamation (RFC 6).

use actix_web::error::ErrorInternalServerError;
use actix_web::Error;
use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::Arc;

use super::blob_io::{read_blob, write_blob, BlobPlacement};
use super::bridge;
use crate::storage::kv_engine::{BitcaskEngine, KvEngine};
use crate::storage::Storage;

/// Each reservation covers this many more addresses than the write needs.
const RESERVE_STEP: u64 = 1 << 30;
const SLOT_SHIFT: u32 = 56;
const COUNTER_MASK: u64 = (1 << SLOT_SHIFT) - 1;

/// The location-store bucket holding `bucket`'s blobs. Bucket names can't
/// contain `\u{1}`, so this never collides with a real bucket.
pub fn blob_bucket(bucket: &str) -> String {
    format!("\u{1}blobs\u{1}{bucket}")
}

/// Zero-padded hex, so key order is address order.
pub fn blob_key(base: u64) -> String {
    format!("{base:016x}")
}

/// This node's slot: `WARPDRIVE_NODE_SLOT`, else its position in
/// `WARPDRIVE_PEERS` (matched against `WARPDRIVE_SELF_ADDR`).
pub fn node_slot() -> Result<u64, String> {
    if let Ok(v) = std::env::var("WARPDRIVE_NODE_SLOT") {
        return v.trim().parse::<u64>().ok().filter(|s| *s < 256).ok_or_else(|| format!("WARPDRIVE_NODE_SLOT must be 0-255, got {v:?}"));
    }
    let me = std::env::var("WARPDRIVE_SELF_ADDR").map_err(|_| "WARPDRIVE_SELF_ADDR is required to pick this node's slot".to_string())?;
    let me = me.trim().trim_end_matches('/');
    let peers = std::env::var("WARPDRIVE_PEERS").unwrap_or_default();
    peers
        .split(',')
        .map(|p| p.trim().trim_end_matches('/'))
        .filter(|p| !p.is_empty())
        .position(|p| p == me)
        .map(|i| i as u64)
        .filter(|i| *i < 256)
        .ok_or_else(|| format!("{me} is not in WARPDRIVE_PEERS, so it has no slot (set WARPDRIVE_NODE_SLOT)"))
}

/// A blob as an object's metadata row carries it.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BlobRef {
    pub user_id: String,
    pub bucket: String,
    pub base: u64,
    pub placement: BlobPlacement,
}

type IndexMap = HashMap<(String, String), std::collections::BTreeMap<u64, BlobPlacement>>;

static INDEX: std::sync::LazyLock<parking_lot::RwLock<IndexMap>> = std::sync::LazyLock::new(|| parking_lot::RwLock::new(HashMap::new()));

/// Adds a blob to this node's address index.
pub fn index_blob(b: &BlobRef) {
    INDEX.write().entry((b.user_id.clone(), b.bucket.clone())).or_default().insert(b.base, b.placement.clone());
}

/// The blob whose range holds `offset`, as `(start address, placement)`.
fn lookup(user_id: &str, bucket: &str, offset: u64) -> Option<(u64, BlobPlacement)> {
    let index = INDEX.read();
    let (base, p) = index.get(&(user_id.to_string(), bucket.to_string()))?.range(..=offset).next_back()?;
    (offset < base + p.len.max(1)).then(|| (*base, p.clone()))
}

/// The blob starting at exactly `base`, for attaching to a metadata row.
pub fn blob_at(user_id: &str, bucket: &str, base: u64) -> Option<BlobRef> {
    let placement = INDEX.read().get(&(user_id.to_string(), bucket.to_string()))?.get(&base)?.clone();
    Some(BlobRef { user_id: user_id.to_string(), bucket: bucket.to_string(), base, placement })
}

/// Hands out address ranges for one node.
struct Allocator {
    slot: u64,
    /// Durable high-water mark per bucket: no address at or above it has
    /// been handed out.
    reserved: BitcaskEngine<u64, HashMap<String, u64>>,
    /// Next free address and current reservation, per bucket.
    next: Mutex<HashMap<String, (u64, u64)>>,
}

impl Allocator {
    fn open(path: &str, slot: u64) -> io::Result<Self> {
        Ok(Self { slot, reserved: BitcaskEngine::open(path, "blob allocator")?, next: Mutex::new(HashMap::new()) })
    }

    /// A range of `len` addresses no other write will ever get.
    fn allocate(&self, user_id: &str, bucket: &str, len: u64) -> io::Result<u64> {
        let key = format!("{}:{user_id}:{bucket}", user_id.len());
        let mut next = self.next.lock();
        let (cur, mut limit) = *next.entry(key.clone()).or_insert_with(|| {
            let hwm = self.reserved.get(&key).unwrap_or(0);
            (hwm, hwm)
        });
        if cur + len > limit {
            limit = cur + len + RESERVE_STEP;
            if limit > COUNTER_MASK {
                return Err(io::Error::other(format!("address space exhausted for bucket {bucket}")));
            }
            // Durable before any address below it is used, so a restart
            // starts at or above every address handed out.
            self.reserved.put(key.clone(), limit)?;
        }
        next.insert(key, (cur + len, limit));
        Ok((self.slot << SLOT_SHIFT) | cur)
    }
}

/// Recently read blobs, so a GET streaming one object in slices fetches the
/// blob once, not once per slice.
struct BlobCache {
    budget: usize,
    used: usize,
    entries: VecDeque<((String, String, String), Arc<Vec<u8>>)>,
}

impl BlobCache {
    fn get(&mut self, k: &(String, String, String)) -> Option<Arc<Vec<u8>>> {
        let pos = self.entries.iter().position(|(key, _)| key == k)?;
        let entry = self.entries.remove(pos)?;
        let bytes = Arc::clone(&entry.1);
        self.entries.push_back(entry);
        Some(bytes)
    }

    fn insert(&mut self, k: (String, String, String), v: Arc<Vec<u8>>) {
        if v.len() > self.budget {
            return;
        }
        self.used += v.len();
        self.entries.push_back((k, v));
        while self.used > self.budget {
            match self.entries.pop_front() {
                Some((_, old)) => self.used -= old.len(),
                None => break,
            }
        }
    }
}

pub struct ClusterBlobStore {
    alloc: Allocator,
    cache: Mutex<BlobCache>,
}

impl ClusterBlobStore {
    /// `alloc_log` holds this node's address reservations.
    pub fn open(alloc_log: &str) -> Result<Self, String> {
        let slot = node_slot()?;
        let alloc = Allocator::open(alloc_log, slot).map_err(|e| format!("blob allocator log {alloc_log}: {e}"))?;
        log::info!("cluster blob store: node slot {slot}");
        // Recently read blobs, `WARPDRIVE_BLOB_CACHE_MB` (default 256, 0 disables).
        let budget_mb: usize = std::env::var("WARPDRIVE_BLOB_CACHE_MB").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(256);
        Ok(Self { alloc, cache: Mutex::new(BlobCache { budget: budget_mb << 20, used: 0, entries: VecDeque::new() }) })
    }

    /// The bytes of the blob holding `offset`, with its start address.
    fn bytes_at(&self, user_id: &str, bucket: &str, offset: u64) -> Result<(u64, Arc<Vec<u8>>), Error> {
        let (base, placement) =
            lookup(user_id, bucket, offset).ok_or_else(|| ErrorInternalServerError(format!("no stored range holds offset {offset:#x} in {bucket}")))?;
        let cache_key = (user_id.to_string(), bucket.to_string(), format!("{base:x}@{}", placement.wire_key));
        if let Some(bytes) = self.cache.lock().get(&cache_key) {
            return Ok((base, bytes));
        }
        let state = bridge::state().ok_or_else(|| ErrorInternalServerError("cluster state not installed"))?;
        let (user, ib) = (user_id.to_string(), blob_bucket(bucket));
        let bytes = bridge::run(move || async move { read_blob(&state, &user, &ib, &placement).await.map_err(|e| e.to_string()) })
            .map_err(ErrorInternalServerError)?;
        let bytes = Arc::new(bytes);
        self.cache.lock().insert(cache_key, Arc::clone(&bytes));
        Ok((base, bytes))
    }
}

impl Storage for ClusterBlobStore {
    fn write(&self, user_id: &str, bucket: &str, data: &[u8]) -> Result<(u64, u64), Error> {
        let len = data.len() as u64;
        // An empty write still takes one address, so every blob has its
        // own key.
        let base = self.alloc.allocate(user_id, bucket, len.max(1)).map_err(ErrorInternalServerError)?;
        let state = bridge::state().ok_or_else(|| ErrorInternalServerError("cluster state not installed"))?;
        let (user, ib, key, body) = (user_id.to_string(), blob_bucket(bucket), blob_key(base), data.to_vec());
        let placement = bridge::run(move || async move { write_blob(&state, &user, &ib, &key, &body).await.map_err(|e| e.to_string()) })
            .map_err(ErrorInternalServerError)?;
        index_blob(&BlobRef { user_id: user_id.to_string(), bucket: bucket.to_string(), base, placement });
        Ok((base, len))
    }

    fn read(&self, user_id: &str, bucket: &str, offset: u64, size: u64) -> Result<Vec<u8>, Error> {
        if size == 0 {
            return Ok(Vec::new());
        }
        let (base, bytes) = self.bytes_at(user_id, bucket, offset)?;
        let start = (offset - base) as usize;
        let end = start + size as usize;
        if end > bytes.len() {
            return Err(ErrorInternalServerError(format!(
                "range {offset:#x}+{size} runs past its blob at {base:#x} ({} bytes)",
                bytes.len()
            )));
        }
        Ok(bytes[start..end].to_vec())
    }

    fn delete(&self, _user_id: &str, bucket: &str, offset_size_list: &[(u64, u64)]) -> Result<(), Error> {
        log::trace!("cluster blob store: {} range(s) in {bucket} unreferenced, kept until space reclamation", offset_size_list.len());
        Ok(())
    }

    fn verify(&self, user_id: &str, bucket: &str, offset: u64, size: u64, checksum: &[u8]) -> Result<bool, Error> {
        use sha2::{Digest, Sha256};
        let data = self.read(user_id, bucket, offset, size)?;
        Ok(Sha256::digest(&data).as_slice() == checksum)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_never_overlap_and_a_restart_never_reuses_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("alloc.log").to_string_lossy().into_owned();
        let mut seen = Vec::new();
        {
            let a = Allocator::open(&path, 3).unwrap();
            for len in [10u64, 1, 5000, 1 << 31] {
                seen.push((a.allocate("u", "b", len).unwrap(), len));
            }
        }
        let a = Allocator::open(&path, 3).unwrap();
        seen.push((a.allocate("u", "b", 7).unwrap(), 7));
        for (i, (base, len)) in seen.iter().enumerate() {
            assert_eq!(base >> SLOT_SHIFT, 3, "slot in the top bits");
            for (other, olen) in &seen[i + 1..] {
                assert!(base + len <= *other || other + olen <= *base, "ranges overlap");
            }
        }
        assert!(seen.last().unwrap().0 > seen[seen.len() - 2].0, "after a restart, addresses only move forward");
    }

    #[test]
    fn buckets_and_slots_get_separate_address_ranges() {
        let dir = tempfile::tempdir().unwrap();
        let a = Allocator::open(&dir.path().join("a.log").to_string_lossy(), 0).unwrap();
        let b = Allocator::open(&dir.path().join("b.log").to_string_lossy(), 1).unwrap();
        assert_eq!(a.allocate("u", "b1", 10).unwrap(), 0);
        assert_eq!(a.allocate("u", "b2", 10).unwrap(), 0, "each bucket has its own address space");
        assert_eq!(b.allocate("u", "b1", 10).unwrap(), 1 << SLOT_SHIFT, "another node's slot never collides");
    }

    #[test]
    fn blob_keys_sort_in_address_order() {
        let mut keys: Vec<String> = [5u64, 1 << 40, 300, 1 << 57].iter().map(|b| blob_key(*b)).collect();
        keys.sort();
        assert_eq!(keys, [5u64, 300, 1 << 40, 1 << 57].iter().map(|b| blob_key(*b)).collect::<Vec<_>>());
    }
}
