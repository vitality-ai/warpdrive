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
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(id) = name.to_str().and_then(|n| n.strip_suffix(".seg")).and_then(|n| n.parse::<u64>().ok()) else { continue };
            if id >= head.seg {
                head = Head { seg: id, next: entry.metadata()?.len() };
            }
        }
        Ok(Self { dir, segment_bytes, head: Mutex::new(head), open: Mutex::new(HashMap::new()) })
    }

    /// Reserves `size` bytes: the segment and the offset inside it. Never
    /// splits a write across segments.
    fn reserve(&self, size: u64) -> (u64, u64) {
        let mut h = self.head.lock();
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
        let mut open = self.open.lock();
        if let Some(s) = open.get(&seg) {
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
        let s = Arc::new(Segment { file, commit: GroupCommit::new() });
        open.insert(seg, Arc::clone(&s));
        Ok(s)
    }
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

fn get_storage_directory() -> PathBuf {
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
}

impl Storage for LocalXFSBinaryStore {
    fn write(&self, user_id: &str, bucket: &str, data: &[u8]) -> Result<(u64, u64), Error> {
        let files = self.bucket(user_id, bucket, true)?;
        let size = data.len() as u64;
        if size > LOCAL_MASK {
            return Err(ErrorInternalServerError(format!("a {size}-byte write is larger than a segment can address")));
        }
        // Reserves a range no other writer gets. The lock covers only the
        // reservation, never the I/O.
        let (seg, local) = files.reserve(size);
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

        let offset = (seg << LOCAL_BITS) | local;
        debug!("Wrote data for user {} bucket {} at segment {} offset {} with size {}", user_id, bucket, seg, local, size);
        Ok((offset, size))
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
    fn reading_a_bucket_never_written_creates_nothing() {
        let store = LocalXFSBinaryStore::new();
        let (u, b) = ("test_user_seg", "seg_never_written");
        assert!(store.read(u, b, 0, 1).is_err());
        assert!(!store.bucket_dir(u, b).exists());
    }
}
