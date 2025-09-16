//! Local XFS binary storage implementation

use crate::storage::Storage;
use std::fs::{OpenOptions, File};
use std::io::{self, Read, Write, Seek, SeekFrom};
use std::path::PathBuf;
use std::env;
use std::collections::HashMap;
use actix_web::Error;
use actix_web::error::{ErrorInternalServerError, ErrorNotFound};
use log::{warn, info};
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::hash::{Hash, Hasher};

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
pub struct LocalXFSBinaryStore {
    // In-memory mapping of user_id -> object_id -> (offset, size)
    object_index: Arc<Mutex<HashMap<String, HashMap<String, (u64, u64)>>>>,
}

impl LocalXFSBinaryStore {
    pub fn new() -> Self {
        Self {
            object_index: Arc::new(Mutex::new(HashMap::new())),
        }
    }
    
    /// Get the file path for a user's binary file
    fn get_user_file_path(&self, user_id: &str) -> PathBuf {
        let storage_dir = get_storage_directory();
        storage_dir.join(format!("{}.bin", user_id))
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
    
    /// Log deletion of an object to a JSON file
    fn log_deletion(&self, user_id: &str, object_id: &str, offset: u64, size: u64) -> Result<(), Error> {
        let log_path = format!("{}.json", user_id);
        let mut log_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .map_err(ErrorInternalServerError)?;
            
        let log_entry = json!({
            object_id: {
                "offset_size": [(offset, size)]
            }
        });
        
        log_file.seek(SeekFrom::End(0))
            .map_err(ErrorInternalServerError)?;
        writeln!(log_file, "{}", log_entry.to_string())
            .map_err(ErrorInternalServerError)?;
            
        Ok(())
    }
}

impl Storage for LocalXFSBinaryStore {
    fn put_object(&self, user_id: &str, object_id: &str, data: &[u8]) -> Result<(), Error> {
        // Write data to the binary file
        let (offset, size) = self.append_to_file(user_id, data)
            .map_err(ErrorInternalServerError)?;
        
        // Update the in-memory index
        let mut index = self.object_index.lock().unwrap();
        let user_objects = index.entry(user_id.to_string()).or_insert_with(HashMap::new);
        user_objects.insert(object_id.to_string(), (offset, size));
        
        info!("Stored object {} for user {} at offset {} with size {}", 
              object_id, user_id, offset, size);
        
        Ok(())
    }
    
    fn get_object(&self, user_id: &str, object_id: &str) -> Result<Vec<u8>, Error> {
        // Look up the object in the index
        let index = self.object_index.lock().unwrap();
        
        if let Some(user_objects) = index.get(user_id) {
            if let Some(&(offset, size)) = user_objects.get(object_id) {
                // Read the data from the binary file
                let data = self.read_from_file(user_id, offset, size)
                    .map_err(ErrorInternalServerError)?;
                
                info!("Retrieved object {} for user {} from offset {} with size {}", 
                      object_id, user_id, offset, size);
                
                return Ok(data);
            }
        }
        
        Err(ErrorNotFound(format!(
            "Object {} not found for user {}", 
            object_id, user_id
        )))
    }
    
    fn delete_object(&self, user_id: &str, object_id: &str) -> Result<(), Error> {
        // Look up the object in the index
        let mut index = self.object_index.lock().unwrap();
        
        if let Some(user_objects) = index.get_mut(user_id) {
            if let Some((offset, size)) = user_objects.remove(object_id) {
                // Log the deletion
                drop(index); // Release the lock before calling log_deletion
                self.log_deletion(user_id, object_id, offset, size)?;
                
                info!("Deleted object {} for user {} (was at offset {} with size {})", 
                      object_id, user_id, offset, size);
                
                return Ok(());
            }
        }
        
        Err(ErrorNotFound(format!(
            "Object {} not found for user {}", 
            object_id, user_id
        )))
    }
    
    fn verify_object(&self, user_id: &str, object_id: &str, checksum: &[u8]) -> Result<bool, Error> {
        // Get the object data
        let data = self.get_object(user_id, object_id)?;
        
        // Simple checksum verification using SHA-1 (placeholder implementation)
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        
        let mut hasher = DefaultHasher::new();
        data.hash(&mut hasher);
        let calculated_hash = hasher.finish().to_be_bytes();
        
        // Compare checksums
        let matches = calculated_hash.as_slice() == checksum;
        
        info!("Verified object {} for user {}: checksum matches = {}", 
              object_id, user_id, matches);
        
        Ok(matches)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::hash::{Hash, Hasher};

    #[test]
    fn test_local_xfs_binary_store_basic_operations() {
        let store = LocalXFSBinaryStore::new();
        let user_id = "test_user_local";
        let object_id = "test_object_local";
        let test_data = b"Hello, Local XFS Storage!";
        
        // Test put_object
        store.put_object(user_id, object_id, test_data).unwrap();
        
        // Test get_object
        let retrieved_data = store.get_object(user_id, object_id).unwrap();
        assert_eq!(retrieved_data, test_data);
        
        // Test verify_object (basic implementation)
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        test_data.hash(&mut hasher);
        let checksum = hasher.finish().to_be_bytes();
        assert!(store.verify_object(user_id, object_id, &checksum).unwrap());
        
        // Test delete_object
        store.delete_object(user_id, object_id).unwrap();
        
        // After deletion, get should fail
        assert!(store.get_object(user_id, object_id).is_err());
    }
    
    #[test]
    fn test_local_xfs_binary_store_error_cases() {
        let store = LocalXFSBinaryStore::new();
        let user_id = "test_user_error";
        let object_id = "nonexistent_object";
        
        // Test get_object for nonexistent object
        assert!(store.get_object(user_id, object_id).is_err());
        
        // Test delete_object for nonexistent object
        assert!(store.delete_object(user_id, object_id).is_err());
        
        // Test verify_object for nonexistent object
        let dummy_checksum = [0u8; 8];
        assert!(store.verify_object(user_id, object_id, &dummy_checksum).is_err());
    }
}