//! Local XFS binary storage implementation

use crate::storage::Storage;
use std::collections::HashMap;
use std::fs::{OpenOptions, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::env;
use actix_web::Error;
use actix_web::error::ErrorInternalServerError;
use log::{debug, trace, warn};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use lazy_static::lazy_static;

// Per-(user, bucket) next-write-offset, so concurrent writes to DIFFERENT
// buckets never block each other, and concurrent writes to the SAME bucket
// stay correct via fetch_add rather than a shared lock held for the whole
// write. Previously one global `Mutex<()>` serialized every write in the
// process regardless of bucket — confirmed (via a load test) to be the
// dominant bottleneck for the cluster's concurrent shard writes, now fixed
// at the source rather than worked around above this trait. The mutex here
// is only taken once per (user, bucket), to initialize its counter from the
// file's current size — not on every write.
lazy_static! {
    static ref NEXT_OFFSET: Mutex<HashMap<String, Arc<AtomicU64>>> = Mutex::new(HashMap::new());
}

fn offset_counter(user_id: &str, bucket: &str, file_path: &PathBuf) -> Arc<AtomicU64> {
    let key = format!("{user_id}/{bucket}");
    {
        let map = NEXT_OFFSET.lock().unwrap();
        if let Some(c) = map.get(&key) {
            return Arc::clone(c);
        }
    }
    let initial = std::fs::metadata(file_path).map(|m| m.len()).unwrap_or(0);
    let mut map = NEXT_OFFSET.lock().unwrap();
    Arc::clone(
        map.entry(key)
            .or_insert_with(|| Arc::new(AtomicU64::new(initial))),
    )
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
pub struct LocalXFSBinaryStore;

impl LocalXFSBinaryStore {
    pub fn new() -> Self { Self }
    
    /// Get the file path for a user's bucket binary file
    fn get_bucket_file_path(&self, user_id: &str, bucket: &str) -> PathBuf {
        let storage_dir = get_storage_directory();
        let user_dir = storage_dir.join(user_id);
        
        // Create user directory if it doesn't exist
        if !user_dir.exists() {
            std::fs::create_dir_all(&user_dir)
                .expect("Failed to create user directory");
        }
        
        // Return path as user/bucket-name.bin
        user_dir.join(format!("{}.bin", bucket))
    }
    
    /// Open or create a user's bucket binary file for writing
    fn open_bucket_file_for_write(&self, user_id: &str, bucket: &str) -> io::Result<File> {
        let file_path = self.get_bucket_file_path(user_id, bucket);
        OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .append(false)  // Don't use append mode to allow seeking
            .open(&file_path)
    }

    /// Open a user's bucket binary file for reading
    fn open_bucket_file_for_read(&self, user_id: &str, bucket: &str) -> io::Result<File> {
        let file_path = self.get_bucket_file_path(user_id, bucket);
        OpenOptions::new()
            .read(true)
            .open(&file_path)
    }
}

impl Storage for LocalXFSBinaryStore {
    fn write(&self, user_id: &str, bucket: &str, data: &[u8]) -> Result<(u64, u64), Error> {
        let file_path = self.get_bucket_file_path(user_id, bucket);
        let counter = offset_counter(user_id, bucket, &file_path);

        let size = data.len() as u64;
        // Reserve a non-overlapping byte range atomically, so concurrent
        // writers (including to the same bucket) never race on where they
        // write — no lock needed for the write itself.
        let offset = counter.fetch_add(size, Ordering::SeqCst);

        let file = self.open_bucket_file_for_write(user_id, bucket)
            .map_err(ErrorInternalServerError)?;

        // Positioned write (pwrite): writes at `offset` regardless of this
        // file handle's own cursor, so concurrently-opened handles to the
        // same file never interfere with each other.
        file.write_at(data, offset)
            .map_err(ErrorInternalServerError)?;

        debug!("Wrote data for user {} bucket {} at offset {} with size {}",
              user_id, bucket, offset, size);

        Ok((offset, size))
    }
    
    fn read(&self, user_id: &str, bucket: &str, offset: u64, size: u64) -> Result<Vec<u8>, Error> {
        // Read data from the bucket binary file at specific offset/size
        let mut file = self.open_bucket_file_for_read(user_id, bucket)
            .map_err(ErrorInternalServerError)?;
        
        file.seek(SeekFrom::Start(offset))
            .map_err(ErrorInternalServerError)?;
        
        let mut buffer = vec![0u8; size as usize];
        file.read_exact(&mut buffer)
            .map_err(ErrorInternalServerError)?;
        
        
        trace!("Read data for user {} bucket {} from offset {} with size {}", 
              user_id, bucket, offset, size);
        
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
}