//! Local XFS binary storage implementation

use crate::storage::Storage;
use std::fs::{OpenOptions, File};
use std::io::{self, Read, Write, Seek, SeekFrom};
use std::path::PathBuf;
use std::env;
use actix_web::Error;
use actix_web::error::ErrorInternalServerError;
use log::{warn, info};
use std::sync::Mutex;
use lazy_static::lazy_static;

// Global mutex to synchronize concurrent writes to storage files
lazy_static! {
    static ref STORAGE_WRITE_LOCK: Mutex<()> = Mutex::new(());
}

fn get_storage_directory() -> PathBuf {
    // Try to get the storage directory from environment variable
    match env::var("STORAGE_DIRECTORY") {
        Ok(dir) => {
            info!("Using storage directory from environment: {}", dir);
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
            info!("Using default storage directory: {}", default_path.display());
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
        // Acquire global lock to synchronize concurrent writes
        let _lock = STORAGE_WRITE_LOCK.lock().unwrap();
        
        // Write data to the bucket binary file and return real offset/size
        let mut file = self.open_bucket_file_for_write(user_id, bucket)
            .map_err(ErrorInternalServerError)?;
        
        let offset = file.seek(SeekFrom::End(0))
            .map_err(ErrorInternalServerError)?;
        
        
        file.write_all(data)
            .map_err(ErrorInternalServerError)?;
        
        // Flush to ensure data is written
        file.flush()
            .map_err(ErrorInternalServerError)?;
        
        let size = data.len() as u64;
        
        info!("Wrote data for user {} bucket {} at offset {} with size {}", 
              user_id, bucket, offset, size);
        
        // Lock is automatically released when _lock goes out of scope
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
        
        
        info!("Read data for user {} bucket {} from offset {} with size {}", 
              user_id, bucket, offset, size);
        
        Ok(buffer)
    }
    
    fn delete(&self, user_id: &str, bucket: &str, offset_size_list: &[(u64, u64)]) -> Result<(), Error> {
        // Queue deletion event in SQLite for background worker to process
        use crate::metadata::sqlite_store::SQLiteMetadataStore;
        let metadata_store = SQLiteMetadataStore::new();
        // Key is not part of the low-level contract anymore; deletion is range-based
        metadata_store.queue_deletion(user_id, bucket, "", offset_size_list)?;
        
        info!("Queued deletion event for user {} bucket {} with {} chunks", 
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
}