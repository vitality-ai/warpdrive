//! Local XFS binary storage implementation

use crate::storage::Storage;
use std::fs::{OpenOptions, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::env;
use actix_web::Error;
use actix_web::error::ErrorInternalServerError;
use log::{debug, trace, warn};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};
use lazy_static::lazy_static;

// Per-bucket-file append offset, reserved atomically so concurrent writers
// never need to hold a lock across the actual disk I/O: each writer does a
// single fetch_add to claim a non-overlapping byte range, then writes into
// it with a positioned write (`write_at`, i.e. pwrite) rather than
// seek+write on a shared cursor. POSIX guarantees pwrite to non-overlapping
// regions of the same file from different threads is safe, so this replaces
// what used to be a single global mutex serializing every write server-wide
// (measured: PUT avg ballooned to ~70ms, max ~926ms under T=4 concurrent
// load, vs ~6ms uncontended -- see FIXES_LOG.md #7).
// The map's own mutex is only held for a fast HashMap lookup/insert, never
// across I/O.
lazy_static! {
    static ref BUCKET_FILE_LEN: Mutex<HashMap<PathBuf, Arc<AtomicU64>>> = Mutex::new(HashMap::new());
}

fn get_storage_directory() -> PathBuf {
    // Try to get the storage directory from environment variable
    match env::var("STORAGE_DIRECTORY") {
        Ok(dir) => {
            debug!("Using storage directory from environment: {}", dir);
            PathBuf::from(dir)
        }
        Err(_) => {
            debug!("Storage directory not defined in environment, using ./storage");
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

    /// Get (creating on first access) the atomic append-offset counter for a
    /// bucket file. The map lock is only held for this lookup/insert, never
    /// across any disk I/O.
    fn length_counter(&self, file_path: &PathBuf) -> io::Result<Arc<AtomicU64>> {
        let mut map = BUCKET_FILE_LEN.lock().unwrap();
        if let Some(counter) = map.get(file_path) {
            return Ok(Arc::clone(counter));
        }
        let initial_len = std::fs::metadata(file_path).map(|m| m.len()).unwrap_or(0);
        let counter = Arc::new(AtomicU64::new(initial_len));
        map.insert(file_path.clone(), Arc::clone(&counter));
        Ok(counter)
    }
}

impl Storage for LocalXFSBinaryStore {
    fn write(&self, user_id: &str, bucket: &str, data: &[u8], _slab_hint: Option<&str>) -> Result<(u64, u64), Error> {
        // Reserve a non-overlapping byte range atomically, then write into it
        // with a positioned write (pwrite) instead of seek+write on a shared
        // cursor -- no lock is held across the actual disk I/O, so concurrent
        // writers to the same bucket file no longer serialize behind each
        // other. See the BUCKET_FILE_LEN doc comment above.
        let file_path = self.get_bucket_file_path(user_id, bucket);
        let counter = self.length_counter(&file_path)
            .map_err(ErrorInternalServerError)?;

        let size = data.len() as u64;
        let offset = counter.fetch_add(size, Ordering::SeqCst);

        let file = self.open_bucket_file_for_write(user_id, bucket)
            .map_err(ErrorInternalServerError)?;

        file.write_all_at(data, offset)
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
        let (offset, size) = store.write(user_id, bucket, test_data, None).unwrap();
        
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
}