//! Local XFS-based binary storage implementation

use crate::binary::BinaryStorage;
use std::fs::{OpenOptions, File};
use std::io::{self, Read, Write, Seek, SeekFrom};
use actix_web::Error;
use actix_web::error::ErrorInternalServerError;
use log::{warn, error, info};
use serde_json::json;
use std::path::PathBuf;
use std::env;

/// Get storage directory from environment or use default
fn get_storage_directory() -> PathBuf {
    match env::var("STORAGE_DIRECTORY") {
        Ok(dir) => {
            info!("Using storage directory from environment: {}", dir);
            PathBuf::from(dir)
        }
        Err(_) => {
            warn!("Storage directory not defined in environment");
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

/// Handle for managing binary file operations
struct BinaryFile {
    file: File,
}

impl BinaryFile {
    /// Create a new binary file handler for the given user
    fn new(user_id: &str) -> io::Result<Self> {
        let storage_dir = get_storage_directory();
        let file_path = storage_dir.join(format!("{}.bin", user_id));
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&file_path)?;
        Ok(Self { file })
    }

    /// Write data to the file and return the offset and size
    fn write(&mut self, data: &[u8]) -> io::Result<(u64, u64)> {
        let offset = self.file.seek(SeekFrom::End(0))?;
        self.file.write_all(data)?;
        Ok((offset, data.len() as u64))
    }

    /// Read data from the file at specified offset and size
    fn read(&mut self, offset: u64, size: u64) -> io::Result<Vec<u8>> {
        self.file.seek(SeekFrom::Start(offset))?;
        let mut buffer = vec![0u8; size as usize];
        self.file.read_exact(&mut buffer)?;
        Ok(buffer)
    }
}

/// Handle for managing deletion log files
struct DeleteFile {
    file: File,
}

impl DeleteFile {
    /// Create a new delete file handler for the given user
    fn new(user_id: &str) -> Result<Self, Error> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(format!("{}.json", user_id))
            .map_err(ErrorInternalServerError)?;
        Ok(Self { file })
    }

    /// Log the deletion of an object with its key and offset/size information
    fn delete(&mut self, object_id: &str, offset_size_list: &[(u64, u64)]) -> Result<(), Error> {
        let log_entry = json!({
            object_id: {
                "offset_size": offset_size_list
            }
        });
        
        self.file.seek(SeekFrom::End(0))
            .map_err(ErrorInternalServerError)?;
        writeln!(self.file, "{}", log_entry.to_string())
            .map_err(ErrorInternalServerError)?;
        
        Ok(())
    }
}

/// Local XFS-based binary storage implementation
pub struct LocalXFSBinaryStore;

impl LocalXFSBinaryStore {
    pub fn new() -> Self {
        Self
    }
}

impl BinaryStorage for LocalXFSBinaryStore {
    fn put_object(&self, user_id: &str, _object_id: &str, data: &[u8]) -> Result<(u64, u64), Error> {
        let mut binary_file = BinaryFile::new(user_id)
            .map_err(ErrorInternalServerError)?;
        
        binary_file.write(data)
            .map_err(ErrorInternalServerError)
    }

    fn get_object(&self, user_id: &str, _object_id: &str, offset: u64, size: u64) -> Result<Vec<u8>, Error> {
        let mut binary_file = BinaryFile::new(user_id)
            .map_err(ErrorInternalServerError)?;
        
        binary_file.read(offset, size)
            .map_err(ErrorInternalServerError)
    }

    fn delete_object(&self, user_id: &str, object_id: &str, offset_size_list: &[(u64, u64)]) -> Result<(), Error> {
        let mut delete_file = DeleteFile::new(user_id)?;
        delete_file.delete(object_id, offset_size_list)?;
        
        info!("Deleted and logged data for key: {}", object_id);
        Ok(())
    }

    fn put_objects_batch(&self, user_id: &str, data_list: Vec<&[u8]>) -> Result<Vec<(u64, u64)>, Error> {
        let mut binary_file = BinaryFile::new(user_id)
            .map_err(ErrorInternalServerError)?;
        
        let mut offset_size_list = Vec::new();
        
        for (index, data) in data_list.iter().enumerate() {
            match binary_file.write(data) {
                Ok((offset, size)) => {
                    offset_size_list.push((offset, size));
                    info!("Written object {} at offset {} with size {}", index, offset, size);
                }
                Err(e) => {
                    error!("Failed to write object {} to binary storage: {}", index, e);
                    return Err(ErrorInternalServerError(e));
                }
            }
        }
        
        Ok(offset_size_list)
    }

    fn get_objects_batch(&self, user_id: &str, offset_size_list: &[(u64, u64)]) -> Result<Vec<Vec<u8>>, Error> {
        let mut binary_file = BinaryFile::new(user_id)
            .map_err(ErrorInternalServerError)?;
        
        let mut data_list = Vec::new();
        
        for &(offset, size) in offset_size_list.iter() {
            let data = binary_file.read(offset, size)
                .map_err(ErrorInternalServerError)?;
            data_list.push(data);
        }
        
        Ok(data_list)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn cleanup_test_files(user_id: &str) {
        let storage_dir = get_storage_directory();
        let bin_file = storage_dir.join(format!("{}.bin", user_id));
        let json_file = format!("{}.json", user_id);
        
        let _ = fs::remove_file(bin_file);
        let _ = fs::remove_file(json_file);
    }

    #[test]
    fn test_local_xfs_store_basic_operations() {
        let store = LocalXFSBinaryStore::new();
        let user_id = "test_user_xfs";
        let object_id = "test_object";
        let test_data = b"Hello, World!";

        // Clean up any existing test files
        cleanup_test_files(user_id);

        // Test put_object
        let (offset, size) = store.put_object(user_id, object_id, test_data).unwrap();
        assert_eq!(size, test_data.len() as u64);

        // Test get_object
        let retrieved_data = store.get_object(user_id, object_id, offset, size).unwrap();
        assert_eq!(retrieved_data, test_data);

        // Test delete_object
        let offset_size_list = vec![(offset, size)];
        store.delete_object(user_id, object_id, &offset_size_list).unwrap();

        // Clean up test files
        cleanup_test_files(user_id);
    }

    #[test]
    fn test_local_xfs_store_batch_operations() {
        let store = LocalXFSBinaryStore::new();
        let user_id = "test_user_xfs_batch";
        let test_data_list: Vec<&[u8]> = vec![b"data1", b"data2", b"data3"];

        // Clean up any existing test files
        cleanup_test_files(user_id);

        // Test put_objects_batch
        let offset_size_list = store.put_objects_batch(user_id, test_data_list.clone()).unwrap();
        assert_eq!(offset_size_list.len(), 3);

        // Test get_objects_batch
        let retrieved_data_list = store.get_objects_batch(user_id, &offset_size_list).unwrap();
        assert_eq!(retrieved_data_list.len(), 3);
        
        for (i, data) in retrieved_data_list.iter().enumerate() {
            assert_eq!(data, test_data_list[i]);
        }

        // Clean up test files
        cleanup_test_files(user_id);
    }
}