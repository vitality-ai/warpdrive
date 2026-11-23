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
use std::collections::HashMap;
use std::io;
use std::sync::Arc;

use super::blob_io::{read_blob, write_blob, BlobPlacement};
use super::bridge;
use warp_meta_store::{BitcaskEngine, Engine};
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
/// `WARPDRIVE_PEERS`, found by `WARPDRIVE_SELF_ADDR` or, when that isn't
/// set, by the one peer whose port is this node's `WARPDRIVE_PORT` (every
/// node of a single-host cluster has its own port).
pub fn node_slot() -> Result<u64, String> {
    if let Ok(v) = std::env::var("WARPDRIVE_NODE_SLOT") {
        return v.trim().parse::<u64>().ok().filter(|s| *s < 256).ok_or_else(|| format!("WARPDRIVE_NODE_SLOT must be 0-255, got {v:?}"));
    }
    let peers = std::env::var("WARPDRIVE_PEERS").unwrap_or_default();
    let peers: Vec<&str> = peers.split(',').map(|p| p.trim().trim_end_matches('/')).filter(|p| !p.is_empty()).collect();
    slot_in(&peers, std::env::var("WARPDRIVE_SELF_ADDR").ok().as_deref(), std::env::var("WARPDRIVE_PORT").ok().as_deref())
}

fn slot_in(peers: &[&str], self_addr: Option<&str>, port: Option<&str>) -> Result<u64, String> {
    let found = match self_addr.map(|a| a.trim().trim_end_matches('/')) {
        Some(me) => peers.iter().position(|p| *p == me).ok_or_else(|| format!("{me} is not in WARPDRIVE_PEERS, so it has no slot (set WARPDRIVE_NODE_SLOT)"))?,
        None => {
            let port = port.unwrap_or("9710").trim();
            let suffix = format!(":{port}");
            let matches: Vec<usize> = peers.iter().enumerate().filter(|(_, p)| p.ends_with(&suffix)).map(|(i, _)| i).collect();
            match matches.as_slice() {
                [i] => *i,
                _ => {
                    return Err(format!(
                        "can't tell which of WARPDRIVE_PEERS is this node ({} peers use port {port}): set WARPDRIVE_SELF_ADDR or WARPDRIVE_NODE_SLOT",
                        matches.len()
                    ))
                }
            }
        }
    };
    if found >= 256 {
        return Err(format!("this node is peer #{found}, above the 256 slots: set WARPDRIVE_NODE_SLOT"));
    }
    Ok(found as u64)
}

/// A blob as an object's metadata row carries it.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BlobRef {
    pub user_id: String,
    pub bucket: String,
    pub base: u64,
    pub placement: BlobPlacement,
}

/// Blobs this node wrote whose metadata row hasn't claimed them yet: the
/// handoff from `write`, which knows the placement, to the row that stores
/// it. A row claims (removes) its entries when it's written. An entry whose
/// PUT never wrote a row goes after `HANDOFF_TTL` (the audit sweep frees its
/// shards). So it holds writes in flight, not every object: reads take the
/// placement from the row they read (`Locator`), not from this node's memory.
type Handoff = HashMap<(String, String, u64), (BlobPlacement, std::time::Instant)>;

static HANDOFF: std::sync::LazyLock<parking_lot::Mutex<Handoff>> = std::sync::LazyLock::new(|| parking_lot::Mutex::new(HashMap::new()));

const HANDOFF_TTL: std::time::Duration = std::time::Duration::from_secs(3600);

/// Records a blob just written, for its row to claim.
pub fn hand_off(b: BlobRef) {
    let mut h = HANDOFF.lock();
    // Now and then, drop entries no row claimed.
    if h.len() % 4096 == 4095 {
        h.retain(|_, (_, at)| at.elapsed() < HANDOFF_TTL);
    }
    h.insert((b.user_id, b.bucket, b.base), (b.placement, std::time::Instant::now()));
}

/// The blob this node wrote at `base`, if no row has claimed it yet.
pub fn claim(user_id: &str, bucket: &str, base: u64) -> Option<BlobRef> {
    let (placement, _) = HANDOFF.lock().remove(&(user_id.to_string(), bucket.to_string(), base))?;
    Some(BlobRef { user_id: user_id.to_string(), bucket: bucket.to_string(), base, placement })
}

/// Blobs waiting to be claimed, for tests and metrics.
pub fn handoff_len() -> usize {
    HANDOFF.lock().len()
}

/// A chunk's locator: the blob's reference, from the row it was read from.
pub fn locator(b: &BlobRef) -> crate::storage::Locator {
    crate::storage::Locator(Arc::new(b.clone()))
}

fn located(loc: Option<&crate::storage::Locator>, bucket: &str, offset: u64) -> Result<(u64, BlobPlacement), String> {
    let b = loc
        .and_then(|l| l.0.downcast_ref::<BlobRef>())
        .ok_or_else(|| format!("read of {bucket} at {offset:#x} without its blob's placement (the chunk came from no cluster metadata row)"))?;
    if offset < b.base || offset >= b.base + b.placement.len.max(1) {
        return Err(format!("read of {bucket} at {offset:#x} outside its blob at {:#x}", b.base));
    }
    Ok((b.base, b.placement.clone()))
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
/// blob once, not once per slice. A hash map for lookup plus an LRU order
/// for eviction, both O(1) per access: a linear scan over a full cache of
/// 4 KB blobs (~65,000 entries) took over half of a node's CPU on GCP.
struct BlobCache {
    budget: usize,
    used: usize,
    tick: u64,
    entries: HashMap<(String, String, String), (Arc<Vec<u8>>, u64)>,
    /// Last-use tick -> key, oldest first.
    order: std::collections::BTreeMap<u64, (String, String, String)>,
}

impl BlobCache {
    fn new(budget: usize) -> Self {
        Self { budget, used: 0, tick: 0, entries: HashMap::new(), order: std::collections::BTreeMap::new() }
    }

    fn get(&mut self, k: &(String, String, String)) -> Option<Arc<Vec<u8>>> {
        self.tick += 1;
        let tick = self.tick;
        let (bytes, last) = self.entries.get_mut(k)?;
        let old = std::mem::replace(last, tick);
        let bytes = Arc::clone(bytes);
        if let Some(key) = self.order.remove(&old) {
            self.order.insert(tick, key);
        }
        Some(bytes)
    }

    fn insert(&mut self, k: (String, String, String), v: Arc<Vec<u8>>) {
        if v.len() > self.budget || self.entries.contains_key(&k) {
            return;
        }
        self.tick += 1;
        self.used += v.len();
        self.order.insert(self.tick, k.clone());
        self.entries.insert(k, (v, self.tick));
        while self.used > self.budget {
            let Some((_, oldest)) = self.order.pop_first() else { break };
            if let Some((bytes, _)) = self.entries.remove(&oldest) {
                self.used -= bytes.len();
            }
        }
    }
}

pub struct ClusterBlobStore {
    alloc: Allocator,
    cache: Arc<Mutex<BlobCache>>,
}

impl ClusterBlobStore {
    /// `alloc_log` holds this node's address reservations.
    pub fn open(alloc_log: &str) -> Result<Self, String> {
        let slot = node_slot()?;
        let alloc = Allocator::open(alloc_log, slot).map_err(|e| format!("blob allocator log {alloc_log}: {e}"))?;
        log::info!("cluster blob store: node slot {slot}");
        // Recently read blobs, `WARPDRIVE_BLOB_CACHE_MB` (default 256, 0 disables).
        let budget_mb: usize = std::env::var("WARPDRIVE_BLOB_CACHE_MB").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(256);
        Ok(Self { alloc, cache: Arc::new(Mutex::new(BlobCache::new(budget_mb << 20))) })
    }

    /// The bytes of the blob at `base`.
    fn blob_bytes(&self, user_id: &str, bucket: &str, base: u64, placement: BlobPlacement) -> Result<Arc<Vec<u8>>, Error> {
        let cache_key = (user_id.to_string(), bucket.to_string(), format!("{base:x}@{}", placement.wire_key));
        if let Some(bytes) = self.cache.lock().get(&cache_key) {
            return Ok(bytes);
        }
        let state = bridge::state().ok_or_else(|| ErrorInternalServerError("cluster state not installed"))?;
        let (user, ib) = (user_id.to_string(), blob_bucket(bucket));
        let bytes = bridge::run(move || async move { read_blob(&state, &user, &ib, &placement).await.map_err(|e| e.to_string()) })
            .map_err(ErrorInternalServerError)?;
        let bytes = Arc::new(bytes);
        self.cache.lock().insert(cache_key, Arc::clone(&bytes));
        Ok(bytes)
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
        hand_off(BlobRef { user_id: user_id.to_string(), bucket: bucket.to_string(), base, placement });
        Ok((base, len))
    }

    /// Every cluster read needs the chunk's locator (`read_located`).
    fn read(&self, user_id: &str, bucket: &str, offset: u64, size: u64) -> Result<Vec<u8>, Error> {
        self.read_located(user_id, bucket, offset, size, None)
    }

    fn read_located(&self, user_id: &str, bucket: &str, offset: u64, size: u64, loc: Option<&crate::storage::Locator>) -> Result<Vec<u8>, Error> {
        if size == 0 {
            return Ok(Vec::new());
        }
        let (base, placement) = located(loc, bucket, offset).map_err(ErrorInternalServerError)?;
        let bytes = self.blob_bytes(user_id, bucket, base, placement)?;
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

    /// Reserves the addresses in place, then returns the shard writes as a
    /// future the PUT awaits on its HTTP worker. Each shard write goes over
    /// the network, a node's own included, so the future never blocks.
    fn write_async(&self, user_id: &str, bucket: &str, data: Vec<u8>) -> Option<futures::future::LocalBoxFuture<'static, Result<(u64, u64), String>>> {
        let len = data.len() as u64;
        let base = match self.alloc.allocate(user_id, bucket, len.max(1)) {
            Ok(b) => b,
            Err(e) => return Some(Box::pin(std::future::ready(Err(e.to_string())))),
        };
        let Some(state) = bridge::state() else {
            return Some(Box::pin(std::future::ready(Err("cluster state not installed".to_string()))));
        };
        let (user, bucket) = (user_id.to_string(), bucket.to_string());
        Some(Box::pin(async move {
            let placement = write_blob(&state, &user, &blob_bucket(&bucket), &blob_key(base), &data).await.map_err(|e| e.to_string())?;
            hand_off(BlobRef { user_id: user, bucket, base, placement });
            Ok((base, len))
        }))
    }

    /// A cache hit answers in place. A miss returns the shard reads as a
    /// future, so a GET awaits them on its HTTP worker instead of blocking
    /// a pool thread that blocks a bridge thread.
    fn read_async(&self, user_id: &str, bucket: &str, offset: u64, size: u64) -> Option<futures::future::LocalBoxFuture<'static, Result<Vec<u8>, String>>> {
        self.read_located_async(user_id, bucket, offset, size, None)
    }

    fn read_located_async(&self, user_id: &str, bucket: &str, offset: u64, size: u64, loc: Option<&crate::storage::Locator>) -> Option<futures::future::LocalBoxFuture<'static, Result<Vec<u8>, String>>> {
        let slice = move |base: u64, bytes: &[u8]| -> Result<Vec<u8>, String> {
            let start = (offset - base) as usize;
            let end = start + size as usize;
            bytes.get(start..end).map(<[u8]>::to_vec).ok_or_else(|| format!("range {offset:#x}+{size} runs past its blob at {base:#x} ({} bytes)", bytes.len()))
        };
        if size == 0 {
            return Some(Box::pin(std::future::ready(Ok(Vec::new()))));
        }
        let (base, placement) = match located(loc, bucket, offset) {
            Ok(x) => x,
            Err(e) => return Some(Box::pin(std::future::ready(Err(e)))),
        };
        let cache_key = (user_id.to_string(), bucket.to_string(), format!("{base:x}@{}", placement.wire_key));
        if let Some(bytes) = self.cache.lock().get(&cache_key) {
            return Some(Box::pin(std::future::ready(slice(base, &bytes))));
        }
        let Some(state) = bridge::state() else {
            return Some(Box::pin(std::future::ready(Err("cluster state not installed".to_string()))));
        };
        let (user, ib, cache) = (user_id.to_string(), blob_bucket(bucket), Arc::clone(&self.cache));
        Some(Box::pin(async move {
            let bytes = Arc::new(read_blob(&state, &user, &ib, &placement).await.map_err(|e| e.to_string())?);
            cache.lock().insert(cache_key, Arc::clone(&bytes));
            slice(base, &bytes)
        }))
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
    fn a_row_claims_its_blob_once() {
        let p = BlobPlacement { shard_peers: vec!["a".into()], k: 1, m: 0, wire_key: "x".into(), len: 10, inline: None };
        hand_off(BlobRef { user_id: "u-claim".into(), bucket: "b-claim".into(), base: 100, placement: p.clone() });
        assert!(claim("u-claim", "other", 100).is_none(), "another bucket's address is another blob");
        assert_eq!(claim("u-claim", "b-claim", 100).map(|b| b.placement), Some(p));
        assert!(claim("u-claim", "b-claim", 100).is_none(), "claimed once");
    }

    #[test]
    fn a_read_needs_the_chunks_locator_and_stays_inside_its_blob() {
        let p = BlobPlacement { shard_peers: vec!["a".into()], k: 1, m: 0, wire_key: "x".into(), len: 10, inline: None };
        let loc = locator(&BlobRef { user_id: "u".into(), bucket: "b".into(), base: 100, placement: p });
        assert!(located(None, "b", 100).is_err());
        assert_eq!(located(Some(&loc), "b", 105).map(|(base, _)| base), Ok(100));
        assert!(located(Some(&loc), "b", 110).is_err());
    }

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
    fn the_slot_comes_from_the_self_address_or_a_unique_port() {
        let peers = ["http://10.0.0.1:9710", "http://10.0.0.2:9710", "http://10.0.0.3:9710"];
        assert_eq!(slot_in(&peers, Some("http://10.0.0.2:9710/"), None), Ok(1));
        assert!(slot_in(&peers, None, Some("9710")).is_err(), "three peers share the port, so it's ambiguous");
        let local = ["http://127.0.0.1:9710", "http://127.0.0.1:9711", "http://127.0.0.1:9712"];
        assert_eq!(slot_in(&local, None, Some("9712")), Ok(2));
        assert!(slot_in(&local, Some("http://127.0.0.1:9999"), None).is_err());
    }

    #[test]
    fn the_cache_evicts_least_recently_used_first_and_stays_within_budget() {
        let k = |n: &str| ("u".to_string(), "b".to_string(), n.to_string());
        let mut c = BlobCache::new(30);
        c.insert(k("a"), Arc::new(vec![0; 10]));
        c.insert(k("b"), Arc::new(vec![0; 10]));
        c.insert(k("c"), Arc::new(vec![0; 10]));
        assert!(c.get(&k("a")).is_some(), "touching a makes b the oldest");
        c.insert(k("d"), Arc::new(vec![0; 10]));
        assert!(c.get(&k("b")).is_none(), "b was evicted");
        assert!(c.get(&k("a")).is_some() && c.get(&k("c")).is_some() && c.get(&k("d")).is_some());
        assert_eq!(c.used, 30);
        c.insert(k("huge"), Arc::new(vec![0; 31]));
        assert!(c.get(&k("huge")).is_none(), "a blob over the budget isn't cached");
    }

    #[test]
    fn blob_keys_sort_in_address_order() {
        let mut keys: Vec<String> = [5u64, 1 << 40, 300, 1 << 57].iter().map(|b| blob_key(*b)).collect();
        keys.sort();
        assert_eq!(keys, [5u64, 300, 1 << 40, 1 << 57].iter().map(|b| blob_key(*b)).collect::<Vec<_>>());
    }
}
