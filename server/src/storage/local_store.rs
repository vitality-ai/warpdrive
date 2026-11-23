//! Local XFS binary storage implementation

use crate::storage::Storage;
use std::collections::HashMap;
use std::fs::{OpenOptions, File};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::env;
use actix_web::Error;
use actix_web::error::ErrorInternalServerError;
use log::{debug, trace, warn};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use parking_lot::Mutex;
use lazy_static::lazy_static;
use warp_meta_store::group_commit::GroupCommit;

// Each (user, bucket) stores its bytes in segment files under
// `{storage}/{user}/{bucket}/`, named `{id:010}.seg` (RFC 6, step 3). Writes
// append to the active segment. When the next write would take it past the
// segment size, the next segment becomes active. A write larger than a
// segment gets one to itself. Sealed segments are never written again, so
// the compactor can copy their live bytes out and delete them whole.
//
// The offset this store hands out is global: the segment id in the top
// 24 bits and the offset inside the segment in the low 40. The shard index
// keeps storing one (offset, size) pair, and a read finds its segment
// without a lookup.
const LOCAL_BITS: u32 = 40;
const LOCAL_MASK: u64 = (1 << LOCAL_BITS) - 1;

/// The segment a global offset points into.
pub fn segment_of(offset: u64) -> u64 {
    offset >> LOCAL_BITS
}

fn local_of(offset: u64) -> u64 {
    offset & LOCAL_MASK
}

/// Segment ids from here up hold survivors: live bytes the compactor copied
/// out of older segments (RFC 6, step 4). Keeping them apart from fresh
/// writes groups long-lived data, so survivor segments rarely need copying
/// again.
pub const SURVIVOR_BASE: u64 = 1 << 23;

/// One segment as the compactor sees it.
#[derive(Debug, Clone)]
pub struct SegmentInfo {
    pub id: u64,
    pub len: u64,
    /// Time since the segment was last written.
    pub idle: std::time::Duration,
    /// Still taking writes (one of the two heads): never compacted.
    pub active: bool,
    /// Bytes the file takes on disk: `len` less any punched holes.
    pub physical: u64,
}

/// The filesystem block size holes are punched in.
const BLOCK: u64 = 4096;

/// Frees the disk blocks of `[offset, offset + len)`, keeping the file's
/// length. Reads of the range return zeros afterwards.
#[cfg(target_os = "linux")]
fn punch_hole(file: &File, offset: u64, len: u64) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let rc = unsafe { libc::fallocate(file.as_raw_fd(), libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE, offset as libc::off_t, len as libc::off_t) };
    if rc == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

#[cfg(target_os = "macos")]
fn punch_hole(file: &File, offset: u64, len: u64) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let arg = libc::fpunchhole_t { fp_flags: 0, reserved: 0, fp_offset: offset as libc::off_t, fp_length: len as libc::off_t };
    let rc = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_PUNCHHOLE, &arg) };
    if rc == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn punch_hole(_file: &File, _offset: u64, _len: u64) -> io::Result<()> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "hole punching"))
}

/// Copies `len` bytes from `src` at `from` to `dst` at `to` inside the
/// kernel: no copy through user space, and on XFS with reflink the blocks
/// are shared, not copied. Fails rather than copy fewer bytes.
#[cfg(target_os = "linux")]
fn copy_range(src: &File, from: u64, dst: &File, to: u64, len: u64) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let (mut i, mut o, mut left) = (from as libc::loff_t, to as libc::loff_t, len as usize);
    while left > 0 {
        let n = unsafe { libc::copy_file_range(src.as_raw_fd(), &mut i, dst.as_raw_fd(), &mut o, left, 0) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, format!("copy_file_range stopped with {left} bytes left")));
        }
        left -= n as usize;
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn copy_range(src: &File, from: u64, dst: &File, to: u64, len: u64) -> io::Result<()> {
    let mut buf = vec![0u8; len as usize];
    src.read_exact_at(&mut buf, from)?;
    dst.write_all_at(&buf, to)
}

/// Bytes after which a segment is sealed. `WARPDRIVE_SEGMENT_MB`, default
/// 64: the RFC 6 matrix freed the most space on small objects at 64 MB, and
/// 64 to 128 MB kept the compaction unit small at no cost in write p99.
fn default_segment_bytes() -> u64 {
    static BYTES: std::sync::LazyLock<u64> = std::sync::LazyLock::new(|| {
        env::var("WARPDRIVE_SEGMENT_MB").ok().and_then(|v| v.parse::<u64>().ok()).filter(|n| *n > 0).unwrap_or(64) << 20
    });
    *BYTES
}

/// One segment file and the group commit that batches its fsyncs. An fsync
/// covers one file, so each segment has its own batching domain.
struct Segment {
    file: File,
    commit: GroupCommit,
    /// `TICK` at the last use, for evicting the least recently used.
    used: AtomicU64,
}

// Open segment handles are capped, so descriptors don't grow with the data
// a node stores (about 16 per GB at 64 MB segments). Past the cap the least
// recently used sealed segments leave the cache. A reader or writer still
// holding one keeps using it, and the file closes when the last one drops
// it. The two write heads are never evicted. A cold segment's next read
// opens it again: one open(2).
static OPEN: AtomicUsize = AtomicUsize::new(0);
static TICK: AtomicU64 = AtomicU64::new(0);

/// Segment handles kept open: 4,096, or half the open-file limit if that's
/// lower. `raise_open_file_limit` runs first at startup.
fn open_cap() -> usize {
    static CAP: std::sync::LazyLock<usize> = std::sync::LazyLock::new(|| {
        let soft = open_file_limit().unwrap_or(1024) as usize;
        (soft / 2).clamp(64, 4096)
    });
    *CAP
}

fn open_file_limit() -> Option<u64> {
    let mut lim = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    (unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } == 0).then_some(lim.rlim_cur as u64)
}

/// Raises the soft open-file limit to the hard one, as the Go runtime does,
/// and returns `(soft, hard)` afterwards. A service started by systemd or
/// over SSH gets a soft limit of 1,024 otherwise.
pub fn raise_open_file_limit() -> io::Result<(u64, u64)> {
    let mut lim = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // macOS refuses a soft limit above OPEN_MAX (10,240) even when the hard
    // limit is unlimited.
    #[cfg(target_os = "macos")]
    let want = lim.rlim_max.min(10_240);
    #[cfg(not(target_os = "macos"))]
    let want = lim.rlim_max;
    if lim.rlim_cur < want {
        let raised = libc::rlimit { rlim_cur: want, rlim_max: lim.rlim_max };
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raised) } != 0 {
            return Err(io::Error::last_os_error());
        }
        lim.rlim_cur = want;
    }
    Ok((lim.rlim_cur as u64, lim.rlim_max as u64))
}

/// Drops the least recently used sealed segments from every bucket's cache
/// until at most 90% of the cap is left.
fn evict_cold_segments() {
    let cap = open_cap();
    if OPEN.load(Ordering::Relaxed) <= cap {
        return;
    }
    let buckets: Vec<Arc<BucketFiles>> = BUCKETS.lock().values().cloned().collect();
    let mut cold: Vec<(u64, usize, u64)> = Vec::new();
    for (bi, b) in buckets.iter().enumerate() {
        let (fresh, survivor) = b.heads();
        for (&id, s) in b.open.lock().iter() {
            if id != fresh && id != survivor {
                cold.push((s.used.load(Ordering::Relaxed), bi, id));
            }
        }
    }
    cold.sort_unstable();
    let excess = OPEN.load(Ordering::Relaxed).saturating_sub(cap * 9 / 10);
    for (_, bi, id) in cold.into_iter().take(excess) {
        if buckets[bi].open.lock().remove(&id).is_some() {
            OPEN.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

/// Segment handles open now, for tests and metrics.
pub fn open_segments() -> usize {
    OPEN.load(Ordering::Relaxed)
}

/// Where the next write in a bucket goes.
struct Head {
    seg: u64,
    next: u64,
}

struct BucketFiles {
    dir: PathBuf,
    segment_bytes: u64,
    head: Mutex<Head>,
    survivor: Mutex<Head>,
    // Every segment opened so far stays open: reads are pread on a shared
    // handle. 64 MB segments mean about 16 descriptors per GB stored.
    open: Mutex<HashMap<u64, Arc<Segment>>>,
}

fn segment_name(seg: u64) -> String {
    format!("{seg:010}.seg")
}

/// fsync a directory, so an entry just created in it survives a crash.
fn sync_dir(dir: &std::path::Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

impl BucketFiles {
    /// Opens a bucket's directory, or creates it when `create`. The active
    /// segment is the highest-numbered one, and writes continue at its
    /// current length.
    fn open(dir: PathBuf, segment_bytes: u64, create: bool) -> io::Result<Self> {
        if !dir.is_dir() {
            if !create {
                return Err(io::Error::new(io::ErrorKind::NotFound, format!("no segments at {}", dir.display())));
            }
            std::fs::create_dir_all(&dir)?;
            // The new directory's entry must survive a crash along with the
            // first segment written into it.
            if let Some(parent) = dir.parent() {
                sync_dir(parent)?;
                if let Some(root) = parent.parent() {
                    sync_dir(root)?;
                }
            }
        }
        let mut head = Head { seg: 0, next: 0 };
        let mut survivor = Head { seg: SURVIVOR_BASE, next: 0 };
        for (id, len, _, _) in list_segments(&dir)? {
            let h = if id >= SURVIVOR_BASE { &mut survivor } else { &mut head };
            if id >= h.seg {
                *h = Head { seg: id, next: len };
            }
        }
        Ok(Self { dir, segment_bytes, head: Mutex::new(head), survivor: Mutex::new(survivor), open: Mutex::new(HashMap::new()) })
    }

    /// The two segments still taking writes.
    fn heads(&self) -> (u64, u64) {
        (self.head.lock().seg, self.survivor.lock().seg)
    }

    /// Reserves `size` bytes at the fresh head, or the survivor head: the
    /// segment and the offset inside it. Never splits a write across
    /// segments.
    fn reserve(&self, size: u64, survivor: bool) -> (u64, u64) {
        let mut h = if survivor { self.survivor.lock() } else { self.head.lock() };
        if h.next > 0 && h.next + size > self.segment_bytes {
            h.seg += 1;
            h.next = 0;
        }
        let at = (h.seg, h.next);
        h.next += size;
        at
    }

    /// The open segment `seg`. Creating it (on a write) also syncs the
    /// directory before anyone can acknowledge a write into it.
    fn segment(&self, seg: u64, create: bool) -> io::Result<Arc<Segment>> {
        let tick = TICK.fetch_add(1, Ordering::Relaxed);
        let mut open = self.open.lock();
        if let Some(s) = open.get(&seg) {
            s.used.store(tick, Ordering::Relaxed);
            return Ok(Arc::clone(s));
        }
        let path = self.dir.join(segment_name(seg));
        let file = if create {
            let existed = path.exists();
            let f = OpenOptions::new().create(true).read(true).write(true).open(&path)?;
            if !existed {
                sync_dir(&self.dir)?;
            }
            f
        } else {
            OpenOptions::new().read(true).write(true).open(&path)?
        };
        let s = Arc::new(Segment { file, commit: GroupCommit::new(), used: AtomicU64::new(tick) });
        open.insert(seg, Arc::clone(&s));
        drop(open);
        // After this bucket's lock is released: eviction takes every
        // bucket's lock in turn, this one too.
        if OPEN.fetch_add(1, Ordering::Relaxed) + 1 > open_cap() {
            evict_cold_segments();
        }
        Ok(s)
    }
}

/// The segment files in `dir`: id, length, last-write time and bytes on disk.
fn list_segments(dir: &std::path::Path) -> io::Result<Vec<(u64, u64, std::time::SystemTime, u64)>> {
    use std::os::unix::fs::MetadataExt;
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(id) = name.to_str().and_then(|n| n.strip_suffix(".seg")).and_then(|n| n.parse::<u64>().ok()) else { continue };
        let meta = entry.metadata()?;
        out.push((id, meta.len(), meta.modified()?, meta.blocks() * 512));
    }
    out.sort_by_key(|(id, _, _, _)| *id);
    Ok(out)
}

lazy_static! {
    static ref BUCKETS: Mutex<HashMap<PathBuf, Arc<BucketFiles>>> = Mutex::new(HashMap::new());
}

fn bucket_files(dir: PathBuf, segment_bytes: u64, create: bool) -> io::Result<Arc<BucketFiles>> {
    if let Some(b) = BUCKETS.lock().get(&dir) {
        return Ok(Arc::clone(b));
    }
    // Opened outside the map lock: scanning a directory is I/O. Two racing
    // openers build the same state and the first insert wins.
    let fresh = Arc::new(BucketFiles::open(dir.clone(), segment_bytes, create)?);
    Ok(Arc::clone(BUCKETS.lock().entry(dir).or_insert(fresh)))
}

/// The root every user's bucket directories live under.
pub fn get_storage_directory() -> PathBuf {
    // Try to get the storage directory from environment variable
    match env::var("STORAGE_DIRECTORY") {
        Ok(dir) => {
            debug!("Using storage directory from environment: {}", dir);
            PathBuf::from(dir)
        }
        Err(_) => {
            warn!("Storage directory not defined in environment");
            // Use default directory "./storage"            
            let default_path = PathBuf::from("storage");
            if !default_path.exists() {
                std::fs::create_dir_all(&default_path)
                    .expect("Failed to create default storage directory");
            }
            debug!("Using default storage directory: {}", default_path.display());
            default_path
        }
    }
}

/// Local XFS binary storage implementation
pub struct LocalXFSBinaryStore {
    segment_bytes: u64,
}

impl LocalXFSBinaryStore {
    pub fn new() -> Self {
        Self { segment_bytes: default_segment_bytes() }
    }

    /// A store whose buckets seal segments at `segment_bytes`. The size is
    /// fixed when a bucket is first opened in this process.
    pub fn with_segment_bytes(segment_bytes: u64) -> Self {
        Self { segment_bytes: segment_bytes.clamp(1, LOCAL_MASK) }
    }

    /// The directory holding a bucket's segments: `{storage}/{user}/{bucket}/`.
    fn bucket_dir(&self, user_id: &str, bucket: &str) -> PathBuf {
        get_storage_directory().join(user_id).join(bucket)
    }

    fn bucket(&self, user_id: &str, bucket: &str, create: bool) -> Result<Arc<BucketFiles>, Error> {
        bucket_files(self.bucket_dir(user_id, bucket), self.segment_bytes, create).map_err(ErrorInternalServerError)
    }

    /// Appends `data` at the fresh or the survivor head and returns once
    /// it's durable.
    fn append(&self, user_id: &str, bucket: &str, data: &[u8], survivor: bool) -> Result<(u64, u64), Error> {
        let files = self.bucket(user_id, bucket, true)?;
        let size = data.len() as u64;
        if size > LOCAL_MASK {
            return Err(ErrorInternalServerError(format!("a {size}-byte write is larger than a segment can address")));
        }
        // Reserves a range no other writer gets. The lock covers only the
        // reservation, never the I/O.
        let (seg, local) = files.reserve(size, survivor);
        let segment = files.segment(seg, true).map_err(ErrorInternalServerError)?;

        // `write_all_at`, not `write_at`: one pwrite may write fewer bytes
        // than asked (signals, ENOSPC, the ~2 GiB cap), and a short write
        // must not be recorded as a full one.
        segment.file.write_all_at(data, local).map_err(ErrorInternalServerError)?;

        // Acknowledged only once durable. The group commit batches the
        // fsyncs of concurrent writers to this segment into fewer real
        // ones (see `warp_meta_store::group_commit`).
        let ticket = segment.commit.ticket();
        segment.commit.wait_for_durable(ticket, &segment.file).map_err(ErrorInternalServerError)?;
        debug!("Wrote data for user {} bucket {} at segment {} offset {} with size {}", user_id, bucket, seg, local, size);
        Ok(((seg << LOCAL_BITS) | local, size))
    }

    /// Copies a survivor out of a segment being compacted: written at the
    /// survivor head, durable on return.
    pub fn write_survivor(&self, user_id: &str, bucket: &str, data: &[u8]) -> Result<(u64, u64), Error> {
        self.append(user_id, bucket, data, true)
    }

    /// Every segment of a bucket, oldest id first. Empty if the bucket has
    /// none.
    pub fn segments(&self, user_id: &str, bucket: &str) -> io::Result<Vec<SegmentInfo>> {
        let dir = self.bucket_dir(user_id, bucket);
        if !dir.is_dir() {
            return Ok(Vec::new());
        }
        let files = bucket_files(dir.clone(), self.segment_bytes, false)?;
        let (fresh, survivor) = files.heads();
        let now = std::time::SystemTime::now();
        Ok(list_segments(&dir)?
            .into_iter()
            .map(|(id, len, modified, physical)| SegmentInfo {
                id,
                len,
                physical,
                idle: now.duration_since(modified).unwrap_or_default(),
                // A head may not have its file yet: the highest id in each
                // range counts as active too, so it's never retired.
                active: id >= fresh && id < SURVIVOR_BASE || id >= survivor,
            })
            .collect())
    }

    /// Copies a survivor's bytes to the survivor head inside the kernel
    /// (`copy_file_range` on Linux), durable on return.
    pub fn copy_to_survivor(&self, user_id: &str, bucket: &str, offset: u64, size: u64) -> Result<(u64, u64), Error> {
        let files = self.bucket(user_id, bucket, false)?;
        let src = files.segment(segment_of(offset), false).map_err(ErrorInternalServerError)?;
        let (seg, local) = files.reserve(size, true);
        let dst = files.segment(seg, true).map_err(ErrorInternalServerError)?;
        copy_range(&src.file, local_of(offset), &dst.file, local, size).map_err(ErrorInternalServerError)?;
        let ticket = dst.commit.ticket();
        dst.commit.wait_for_durable(ticket, &dst.file).map_err(ErrorInternalServerError)?;
        Ok(((seg << LOCAL_BITS) | local, size))
    }

    /// Frees the whole disk blocks inside each `(offset, len)` range of a
    /// sealed segment, offsets inside the segment. Partial blocks at either
    /// end are kept, so bytes outside the ranges are never touched. The
    /// caller passes only ranges nothing points into: they read back as
    /// zeros. Returns the bytes freed.
    pub fn punch(&self, user_id: &str, bucket: &str, seg: u64, ranges: &[(u64, u64)]) -> io::Result<u64> {
        let files = bucket_files(self.bucket_dir(user_id, bucket), self.segment_bytes, false)?;
        let (fresh, survivor) = files.heads();
        if (seg >= fresh && seg < SURVIVOR_BASE) || seg >= survivor {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("segment {seg} is still taking writes")));
        }
        let segment = files.segment(seg, false)?;
        let mut freed = 0;
        for &(start, len) in ranges {
            let (a, b) = (start.div_ceil(BLOCK) * BLOCK, (start + len) / BLOCK * BLOCK);
            if b > a {
                punch_hole(&segment.file, a, b - a)?;
                freed += b - a;
            }
        }
        Ok(freed)
    }

    /// Deletes a sealed segment once nothing points into it. Refuses a
    /// segment still taking writes.
    pub fn retire(&self, user_id: &str, bucket: &str, seg: u64) -> io::Result<()> {
        let files = bucket_files(self.bucket_dir(user_id, bucket), self.segment_bytes, false)?;
        let (fresh, survivor) = files.heads();
        if (seg >= fresh && seg < SURVIVOR_BASE) || seg >= survivor {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("segment {seg} is still taking writes")));
        }
        // Readers already holding the handle keep reading the unlinked file.
        if files.open.lock().remove(&seg).is_some() {
            OPEN.fetch_sub(1, Ordering::Relaxed);
        }
        match std::fs::remove_file(files.dir.join(segment_name(seg))) {
            Ok(()) => sync_dir(&files.dir),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}

impl Storage for LocalXFSBinaryStore {
    fn write(&self, user_id: &str, bucket: &str, data: &[u8]) -> Result<(u64, u64), Error> {
        self.append(user_id, bucket, data, false)
    }

    fn read(&self, user_id: &str, bucket: &str, offset: u64, size: u64) -> Result<Vec<u8>, Error> {
        let files = self.bucket(user_id, bucket, false)?;
        let segment = files.segment(segment_of(offset), false).map_err(ErrorInternalServerError)?;
        let mut buffer = vec![0u8; size as usize];
        // pread on the shared handle: no seek, no shared cursor.
        segment.file.read_exact_at(&mut buffer, local_of(offset)).map_err(ErrorInternalServerError)?;
        trace!("Read data for user {} bucket {} from offset {} with size {}", user_id, bucket, offset, size);
        Ok(buffer)
    }

    fn delete(&self, user_id: &str, bucket: &str, offset_size_list: &[(u64, u64)]) -> Result<(), Error> {
        // Queue deletion event in SQLite for background worker to process
        use crate::metadata::sqlite_store::SQLiteMetadataStore;
        let metadata_store = SQLiteMetadataStore::new();
        // Key is not part of the low-level contract anymore; deletion is range-based
        metadata_store.queue_deletion(user_id, bucket, "", offset_size_list)?;
        
        debug!("Queued deletion event for user {} bucket {} with {} chunks", 
              user_id, bucket, offset_size_list.len());
        Ok(())
    }

    fn verify(&self, user_id: &str, bucket: &str, offset: u64, size: u64, checksum: &[u8]) -> Result<bool, Error> {
        // Stable integrity: SHA-256 over the data bytes
        let data = self.read(user_id, bucket, offset, size)?;
        use sha2::{Sha256, Digest};
        let mut hasher = Sha256::new();
        hasher.update(&data);
        let calculated = hasher.finalize();
        Ok(calculated.as_slice() == checksum)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_local_xfs_binary_store_basic_operations() {
        let store = LocalXFSBinaryStore::new();
        let user_id = "test_user_local";
        let bucket = "test_bucket";
        let test_data = b"Hello, Local XFS Storage!";
        
        // Test write
        let (offset, size) = store.write(user_id, bucket, test_data).unwrap();
        
        // Test read
        let retrieved_data = store.read(user_id, bucket, offset, size).unwrap();
        assert_eq!(retrieved_data, test_data);
        
        // Test verify (SHA-256)
        use sha2::{Sha256, Digest};
        let mut hasher = Sha256::new();
        hasher.update(&test_data[..]);
        let checksum = hasher.finalize().to_vec();
        assert!(store.verify(user_id, bucket, offset, size, &checksum).unwrap());
        
        // Test delete (range-based)
        store.delete(user_id, bucket, &[(offset, size)]).unwrap();
    }
    
    #[test]
    fn test_local_xfs_binary_store_error_cases() {
        let store = LocalXFSBinaryStore::new();
        let user_id = "test_user_error";
        let bucket = "test_bucket";
        // Reading from non-existent file should error
        assert!(store.read(user_id, bucket, 0, 1).is_err());
    }

    /// The race the atomic-offset fix has to get right: many threads
    /// writing concurrently to the SAME (user, bucket) must each land in a
    /// distinct, non-overlapping byte range, with every byte accounted for
    /// — no lost writes, no overlapping writes, no corruption. This is
    /// exactly the property the old global `Mutex<()>` guaranteed by
    /// serializing everything; this test proves `fetch_add` + `write_at`
    /// gives the same guarantee without the lock.
    #[test]
    fn concurrent_writes_to_same_bucket_never_overlap_or_corrupt() {
        use std::thread;

        let user_id = "test_user_concurrent";
        let bucket = "test_bucket_concurrent";
        let threads = 16;
        let writes_per_thread = 50;
        let payload_len = 37; // deliberately not a round number

        let handles: Vec<_> = (0..threads)
            .map(|t| {
                thread::spawn(move || {
                    let store = LocalXFSBinaryStore::new();
                    let mut extents = Vec::with_capacity(writes_per_thread);
                    for i in 0..writes_per_thread {
                        // Each payload is unique and self-describing (thread id,
                        // index, and a fixed marker byte) so we can later verify
                        // every byte read back belongs to exactly one write.
                        let data = vec![((t * writes_per_thread + i) % 251) as u8; payload_len];
                        let (offset, size) = store.write(user_id, bucket, &data).unwrap();
                        extents.push((offset, size, data));
                    }
                    extents
                })
            })
            .collect();

        let mut all_extents = Vec::new();
        for h in handles {
            all_extents.extend(h.join().unwrap());
        }

        // No two writes may claim overlapping byte ranges.
        all_extents.sort_by_key(|(offset, _, _)| *offset);
        for i in 1..all_extents.len() {
            let (prev_offset, prev_size, _) = &all_extents[i - 1];
            let (offset, _, _) = &all_extents[i];
            assert!(
                prev_offset + prev_size <= *offset,
                "overlapping writes: {:?} and {:?}",
                all_extents[i - 1],
                all_extents[i]
            );
        }

        // Every write must read back exactly what was written, at its own offset.
        let store = LocalXFSBinaryStore::new();
        for (offset, size, expected) in &all_extents {
            let got = store.read(user_id, bucket, *offset, *size).unwrap();
            assert_eq!(&got, expected, "mismatch at offset {offset}");
        }

        assert_eq!(all_extents.len(), threads * writes_per_thread);
    }

    /// Drops a bucket's in-memory state, as a restart would.
    fn restart(store: &LocalXFSBinaryStore, user_id: &str, bucket: &str) {
        BUCKETS.lock().remove(&store.bucket_dir(user_id, bucket));
    }

    #[test]
    fn writes_roll_to_a_new_segment_and_never_straddle_one() {
        let store = LocalXFSBinaryStore::with_segment_bytes(100);
        let (u, b) = ("test_user_seg", "seg_roll_bucket");
        let mut got = Vec::new();
        for i in 0..10u8 {
            let data = vec![i; 37];
            got.push((store.write(u, b, &data).unwrap(), data));
        }
        // 37-byte writes into 100-byte segments: two per segment.
        let segs: Vec<u64> = got.iter().map(|((o, _), _)| segment_of(*o)).collect();
        assert_eq!(segs, vec![0, 0, 1, 1, 2, 2, 3, 3, 4, 4]);
        for ((o, n), data) in &got {
            assert!(local_of(*o) + n <= 100, "write at {o:#x} straddles its segment");
            assert_eq!(&store.read(u, b, *o, *n).unwrap(), data);
        }
        let dir = store.bucket_dir(u, b);
        assert!(dir.join(segment_name(4)).is_file());
    }

    #[test]
    fn a_write_larger_than_a_segment_gets_one_to_itself() {
        let store = LocalXFSBinaryStore::with_segment_bytes(100);
        let (u, b) = ("test_user_seg", "seg_big_bucket");
        let (o1, _) = store.write(u, b, &[1; 10]).unwrap();
        let big = vec![7u8; 250];
        let (o2, n2) = store.write(u, b, &big).unwrap();
        let (o3, _) = store.write(u, b, &[3; 10]).unwrap();
        assert_eq!((segment_of(o1), segment_of(o2), segment_of(o3)), (0, 1, 2));
        assert_eq!(local_of(o2), 0);
        assert_eq!(store.read(u, b, o2, n2).unwrap(), big);
    }

    #[test]
    fn after_a_restart_writes_continue_where_the_active_segment_ends() {
        let store = LocalXFSBinaryStore::with_segment_bytes(100);
        let (u, b) = ("test_user_seg", "seg_restart_bucket");
        let before: Vec<_> = (0..5u8).map(|i| { let d = vec![i; 30]; (store.write(u, b, &d).unwrap(), d) }).collect();
        restart(&store, u, b);
        let (o, n) = store.write(u, b, &[9; 30]).unwrap();
        // 5 x 30 bytes: segment 0 holds 3 (90 bytes), segment 1 holds 2 (60).
        assert_eq!((segment_of(o), local_of(o)), (1, 60));
        for ((o, n), d) in &before {
            assert_eq!(&store.read(u, b, *o, *n).unwrap(), d, "a write before the restart was overwritten");
        }
        assert_eq!(store.read(u, b, o, n).unwrap(), vec![9; 30]);
    }

    #[test]
    fn survivors_go_to_their_own_segments_and_heads_are_never_retired() {
        let store = LocalXFSBinaryStore::with_segment_bytes(100);
        let (u, b) = ("test_user_seg", "seg_survivor_bucket");
        let fresh: Vec<_> = (0..3u8).map(|i| store.write(u, b, &[i; 60]).unwrap()).collect();
        let (so, sn) = store.write_survivor(u, b, &[42; 60]).unwrap();
        assert_eq!(segment_of(so), SURVIVOR_BASE);
        assert_eq!(store.read(u, b, so, sn).unwrap(), vec![42; 60]);
        let segs = store.segments(u, b).unwrap();
        let active: Vec<u64> = segs.iter().filter(|s| s.active).map(|s| s.id).collect();
        assert_eq!(active, vec![2, SURVIVOR_BASE]);
        assert!(store.retire(u, b, 2).is_err(), "the fresh head is refused");
        assert!(store.retire(u, b, SURVIVOR_BASE).is_err(), "the survivor head is refused");
        store.retire(u, b, 0).unwrap();
        assert!(store.read(u, b, fresh[0].0, fresh[0].1).is_err(), "a retired segment is gone");
        assert_eq!(store.read(u, b, fresh[1].0, fresh[1].1).unwrap(), vec![1; 60]);
        // After a restart both heads continue where they were.
        restart(&store, u, b);
        let (o, _) = store.write_survivor(u, b, &[7; 30]).unwrap();
        assert_eq!((segment_of(o), local_of(o)), (SURVIVOR_BASE, 60));
        let (o, _) = store.write(u, b, &[8; 30]).unwrap();
        assert_eq!((segment_of(o), local_of(o)), (2, 60));
    }

    #[test]
    fn punching_frees_whole_dead_blocks_and_never_touches_live_bytes() {
        let store = LocalXFSBinaryStore::with_segment_bytes(1 << 20);
        let (u, b) = ("test_user_seg", "seg_punch_bucket");
        // live 5000 bytes, dead 70000, live 3000, dead 50000, then a big write
        // that seals segment 0.
        let live1 = vec![1u8; 5000];
        let (o1, n1) = store.write(u, b, &live1).unwrap();
        let (od, nd) = store.write(u, b, &vec![2u8; 70_000]).unwrap();
        let live2 = vec![3u8; 3000];
        let (o2, n2) = store.write(u, b, &live2).unwrap();
        let (ot, nt) = store.write(u, b, &vec![4u8; 50_000]).unwrap();
        store.write(u, b, &vec![5u8; 1 << 20]).unwrap();
        let before = store.segments(u, b).unwrap()[0].physical;
        // The dead ranges as the compactor computes them: the gaps between
        // live entries, up to the segment's end.
        let gaps = [(local_of(od), nd), (local_of(ot), nt)];
        let freed = store.punch(u, b, 0, &gaps).unwrap();
        // Rounded inward to whole blocks: [5000, 75000) frees [8192, 73728),
        // [78000, 128000) frees [81920, 126976).
        assert_eq!(freed, (73728 - 8192) + (126976 - 81920));
        assert_eq!(store.read(u, b, o1, n1).unwrap(), live1);
        assert_eq!(store.read(u, b, o2, n2).unwrap(), live2);
        let after = store.segments(u, b).unwrap()[0].physical;
        assert!(after < before, "physical size {before} -> {after}");
        assert!(store.punch(u, b, 1, &gaps).is_err(), "the active segment is refused");
    }

    #[test]
    fn copy_to_survivor_copies_exactly_and_durably() {
        let store = LocalXFSBinaryStore::with_segment_bytes(100);
        let (u, b) = ("test_user_seg", "seg_copy_bucket");
        let data: Vec<u8> = (0..90u8).collect();
        let (o, n) = store.write(u, b, &data).unwrap();
        store.write(u, b, &[9; 50]).unwrap();
        let (c, cn) = store.copy_to_survivor(u, b, o, n).unwrap();
        assert_eq!(segment_of(c), SURVIVOR_BASE);
        assert_eq!(store.read(u, b, c, cn).unwrap(), data);
        restart(&store, u, b);
        assert_eq!(store.read(u, b, c, cn).unwrap(), data, "readable after a restart");
    }

    #[test]
    fn open_handles_stay_under_the_cap_and_evicted_segments_read_back() {
        let store = LocalXFSBinaryStore::with_segment_bytes(64);
        let (u, b) = ("test_user_seg", "seg_lru_bucket");
        let cap = open_cap();
        // Twice the cap in one-write segments.
        let written: Vec<_> = (0..(2 * cap) as u32).map(|i| { let d = i.to_le_bytes().repeat(16); (store.write(u, b, &d).unwrap(), d) }).collect();
        assert!(open_segments() <= cap, "{} handles open, cap {cap}", open_segments());
        // Every segment, evicted ones included, reads back exactly.
        for ((o, n), d) in &written {
            assert_eq!(&store.read(u, b, *o, *n).unwrap(), d);
        }
        assert!(open_segments() <= cap);
    }

    #[test]
    fn the_open_file_limit_is_raised_to_the_hard_limit() {
        let (soft, hard) = raise_open_file_limit().unwrap();
        assert!(soft >= 1024 && soft <= hard);
    }

    #[test]
    fn reading_a_bucket_never_written_creates_nothing() {
        let store = LocalXFSBinaryStore::new();
        let (u, b) = ("test_user_seg", "seg_never_written");
        assert!(store.read(u, b, 0, 1).is_err());
        assert!(!store.bucket_dir(u, b).exists());
    }
}
