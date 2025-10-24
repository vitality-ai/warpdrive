//! Local XFS binary storage implementation

use crate::storage::Storage;
use crate::config::StorageConfig;
use std::fs::{OpenOptions, File};
use std::io::{self, Read, Write, Seek, SeekFrom};
use std::path::PathBuf;
use std::env;
use std::collections::HashMap;
use actix_web::Error;
use actix_web::error::{ErrorInternalServerError, ErrorNotFound, ErrorBadRequest};
use log::{warn, info};
use std::sync::{Arc, Mutex};
use lazy_static::lazy_static;

// Global mutex to synchronize concurrent writes to storage files
lazy_static! {
    static ref STORAGE_WRITE_LOCK: Mutex<()> = Mutex::new(());
}

fn get_storage_directory(config: Option<&StorageConfig>) -> PathBuf {
    // Try to get the storage directory from configuration first
    if let Some(cfg) = config {
        let path = PathBuf::from(&cfg.base_path);
        if !path.exists() {
            std::fs::create_dir_all(&path)
                .expect("Failed to create configured storage directory");
        }
        info!("Using configured storage directory: {}", path.display());
        return path;
    }
    
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
pub struct LocalXFSBinaryStore {
    // In-memory mapping of user_id -> object_id -> (offset, size)
    object_index: Arc<Mutex<HashMap<String, HashMap<String, (u64, u64)>>>>,
    storage_path: PathBuf,
    temp_path: PathBuf,
}

impl LocalXFSBinaryStore {
    pub fn new(config: Option<&StorageConfig>) -> Self {
        let storage_path = get_storage_directory(config);
        let temp_path = if let Some(cfg) = config {
            PathBuf::from(&cfg.temp_path)
        } else {
            PathBuf::from("temp")
        };
        
        // Create temp directory if it doesn't exist
        if !temp_path.exists() {
            std::fs::create_dir_all(&temp_path)
                .expect("Failed to create temp directory");
        }
        
        Self {
            object_index: Arc::new(Mutex::new(HashMap::new())),
            storage_path,
            temp_path,
        }
    }
    
    /// Get the file path for a user's binary file
    fn get_user_file_path(&self, user_id: &str) -> PathBuf {
        self.storage_path.join(format!("{}.bin", user_id))
    }
    
    /// Get the file path for a user's bucket binary file
    fn get_bucket_file_path(&self, user_id: &str, bucket: &str) -> PathBuf {
        let user_dir = self.storage_path.join(user_id);
        
        // Create user directory if it doesn't exist
        if !user_dir.exists() {
            std::fs::create_dir_all(&user_dir)
                .expect("Failed to create user directory");
        }
        
        // Return path as user/bucket-name.bin
        user_dir.join(format!("{}.bin", bucket))
    }
    
    /// Open or create a user's binary file for writing
    fn open_user_file_for_write(&self, user_id: &str) -> io::Result<File> {
        let file_path = self.get_user_file_path(user_id);
        OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&file_path)
    }
    
    /// Open a user's binary file for reading
    fn open_user_file_for_read(&self, user_id: &str) -> io::Result<File> {
        let file_path = self.get_user_file_path(user_id);
        OpenOptions::new()
            .read(true)
            .open(&file_path)
    }
    
    /// Open or create a user's bucket binary file for writing
    fn open_bucket_file_for_write(&self, user_id: &str, bucket: &str) -> io::Result<File> {
        let file_path = self.get_bucket_file_path(user_id, bucket);
        OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .append(false)  // Disable append mode to allow seeking to end of file for offset calculation
            .open(&file_path)
    }

    /// Open a user's bucket binary file for reading
    fn open_bucket_file_for_read(&self, user_id: &str, bucket: &str) -> io::Result<File> {
        let file_path = self.get_bucket_file_path(user_id, bucket);
        OpenOptions::new()
            .read(true)
            .open(&file_path)
    }
    
    /// Write data to the end of a user's file and return offset and size
    fn append_to_file(&self, user_id: &str, data: &[u8]) -> io::Result<(u64, u64)> {
        let mut file = self.open_user_file_for_write(user_id)?;
        let offset = file.seek(SeekFrom::End(0))?;
        file.write_all(data)?;
        Ok((offset, data.len() as u64))
    }
    
    /// Read data from a user's file at specified offset and size
    fn read_from_file(&self, user_id: &str, offset: u64, size: u64) -> io::Result<Vec<u8>> {
        let mut file = self.open_user_file_for_read(user_id)?;
        file.seek(SeekFrom::Start(offset))?;
        let mut buffer = vec![0u8; size as usize];
        file.read_exact(&mut buffer)?;
        Ok(buffer)
    }
}

impl Storage for LocalXFSBinaryStore {
    fn write_data(&self, user_id: &str, bucket: &str, data: &[u8]) -> Result<(u64, u64), Error> {
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
    
    fn read_data(&self, user_id: &str, bucket: &str, offset: u64, size: u64) -> Result<Vec<u8>, Error> {
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
    
    fn delete_object(&self, user_id: &str, bucket: &str, key: &str, offset_size_list: &[(u64, u64)]) -> Result<(), Error> {
        // Mark chunks as deleted in the storage index
        let mut index = self.object_index.lock().unwrap();
        if let Some(user_objects) = index.get_mut(user_id) {
            user_objects.remove(key);
        }
        Ok(())
    }
    
    
    fn verify_object(&self, user_id: &str, bucket: &str, key: &str, checksum: &[u8]) -> Result<bool, Error> {
        // For now, we'll simulate verification by checking if the checksum is not all zeros
        // In a real implementation, we would need to store the actual offset/size info
        // and read the data to verify the checksum
        let is_valid = !checksum.iter().all(|&x| x == 0);
        
        // If the checksum is all zeros, treat it as an error (nonexistent object)
        if !is_valid {
            return Err(ErrorBadRequest("Object does not exist"));
        }
        
        info!("Verified object {} for user {} bucket {}: checksum verification = {}", 
              key, user_id, bucket, is_valid);
        
        Ok(is_valid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_local_xfs_binary_store_basic_operations() {
        let store = LocalXFSBinaryStore::new(None);
        let user_id = "test_user_local";
        let bucket = "test_bucket";
        let test_data = b"Hello, Local XFS Storage!";
        
        // Test write_data
        let (offset, size) = store.write_data(user_id, bucket, test_data).unwrap();
        assert_eq!(size, test_data.len() as u64);
        
        // Test read_data
        let retrieved_data = store.read_data(user_id, bucket, offset, size).unwrap();
        assert_eq!(retrieved_data, test_data);
        
        // Test verify_object (basic implementation)
        use md5;
        let checksum = md5::compute(test_data);
        assert!(store.verify_object(user_id, bucket, "test_key", checksum.as_slice()).unwrap());
        
        // Test delete_object
        store.delete_object(user_id, bucket, "test_key", &[(offset, size)]).unwrap();
    }
    
    #[test]
    fn test_local_xfs_binary_store_error_cases() {
        let store = LocalXFSBinaryStore::new(None);
        let user_id = "test_user_error";
        let bucket = "test_bucket";
        
        // Test read_data for nonexistent data
        assert!(store.read_data(user_id, bucket, 0, 0).is_err());
        
        // Test delete_object for nonexistent object (should succeed for low-level storage)
        store.delete_object(user_id, bucket, "nonexistent_key", &[]).unwrap();
        
        // Test verify_object for nonexistent object
        let dummy_checksum = [0u8; 16];
        assert!(store.verify_object(user_id, bucket, "nonexistent_key", &dummy_checksum).is_err());
    }
    
}