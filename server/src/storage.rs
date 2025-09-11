// storage.rs

use std::fs::{OpenOptions, File};
use std::io::{self, Read, Write, Seek, SeekFrom};
use actix_web::Error;
use log::{warn,error, info};
use flatbuffers::{root, FlatBufferBuilder};
use actix_web::error::{ErrorInternalServerError, ErrorBadRequest};
use serde_json::json;
use std::path::{ PathBuf};
use std::env;


use crate::util::flatbuffer_store_generated::store::{FileDataList, FileData, FileDataArgs, FileDataListArgs};

/// Binary Storage abstraction layer
/// Allows swapping between different storage backends (local filesystem, distributed systems, etc.)
pub trait BinaryStorage {
    /// Write flatbuffer data and return offset/size pairs for stored files
    fn write_data(&self, user_id: &str, data: &[u8]) -> Result<Vec<(u64, u64)>, Error>;
    
    /// Read data using offset/size pairs and return flatbuffer data
    fn read_data(&self, user_id: &str, offset_size_list: Vec<(u64, u64)>) -> Result<Vec<u8>, Error>;
    
    /// Log deletion of data identified by key and offset/size pairs
    fn log_deletion(&self, user_id: &str, key: &str, offset_size_list: Vec<(u64, u64)>) -> Result<(), Error>;
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

/* OpenFile provides operations for interacting with binary (.bin) files:
 * - Creating new files
 * - Reading existing files
 * - Writing data to files
 * - Managing file seek operations
 */struct OpenFile {
    file: File,
}



impl OpenFile {
    /* Creates a new file handler for the given user
     * Returns a Result containing either the file handle or an IO error
     */
    fn new(user: &str) -> io::Result<Self> {
        let storage_dir = get_storage_directory();
        let file_path = storage_dir.join(format!("{}.bin", user));
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&file_path)?;
        Ok(Self { file })
    }
    /* Writes data to the file and returns the offset and size
     * Parameters:
     * - data: Byte slice containing data to be written
     * Returns: Tuple of (offset, size) in bytes
     */    
    fn write(&mut self, data: &[u8]) -> io::Result<(u64, u64)> {
        let offset = self.file.seek(SeekFrom::End(0))?;
        self.file.write_all(data)?; 
        Ok((offset, data.len() as u64))
    }
   /* Reads data from the file at specified offset and size
     * Parameters:
     * - offset: Starting position to read from
     * - size: Number of bytes to read
     * Returns: Vector containing the read bytes
     */
    fn read(&mut self, offset: u64, size: u64) -> io::Result<Vec<u8>> {
        self.file.seek(SeekFrom::Start(offset))?;
        let mut buffer = vec![0u8; size as usize];
        self.file.read_exact(&mut buffer)?;
        Ok(buffer)
    }
}


/* 
Processes incoming flatbuffer data and writes files to storage
Returns a vector of (offset, size) pairs for each written file
*/
pub fn write_files_to_storage(user : &str,body: &[u8])
    -> Result<Vec<(u64, u64)>, Error> {
    // Open the storage file in append mode
    let mut haystack = OpenFile::new(&user)?;

    let mut offset_size_list: Vec<(u64, u64)> = Vec::new();

   // Deserialize binary data into FileDataList using flatbuffer
    let file_data_list = match root::<FileDataList>(&body) {
        Ok(data) => data,
        Err(e) => return Err(ErrorBadRequest(format!("Failed to parse FlatBuffers data: {:?}", e))),
    };
    let files = match file_data_list.files() {
        Some(files) => files,
        None => return Err(ErrorBadRequest("No files found in FlatBuffers data")),
    };
    info!("Deserialized {} files", files.len());

    for (index, file_data) in files.iter().enumerate() {
        let data = match file_data.data() {
            Some(data) => data,
            None => {
                error!("No data in file at index {}", index);
                continue;
            }
        };

        match haystack.write(data.bytes()) {
            Ok((offset, size)) => {
                offset_size_list.push((offset, size));
                info!("Written file {} at offset {} with size {}", index, offset, size);
            }
            Err(e) => {
                error!("Failed to write file {} to haystack: {}", index, e);
                return Err(ErrorInternalServerError(e));
            }
        }
    }

    Ok(offset_size_list)
}

/* Retrieves and combines files from storage into a FlatBuffer
 * 
 * Args:
 *   user: User ID for .bin file access
 *   offset_size_list: Vector of (offset, size) pairs locating files
 * 
 * Returns:
 *   Result<Vec<u8>>: Serialized FlatBuffer with requested files
 *   Error: On file access or buffer creation failure
 */
pub fn get_files_from_storage(user : &str, offset_size_list: Vec<(u64, u64)>)-> Result<Vec<u8>, Error> {
    info!("connecting to .bin and gettting files");
    let mut haystack = OpenFile::new(&user).map_err(ErrorInternalServerError)?;
    info!("connected to .bin");
    let mut builder = FlatBufferBuilder::new();
    let mut file_data_vec = Vec::new();
    info!("building the flatbuffer to share");
    for &(offset, size) in offset_size_list.iter() {
        let data = haystack.read(offset, size).map_err(ErrorInternalServerError)?;
        let data_vector = builder.create_vector(&data);
        let file_data = FileData::create(&mut builder, &FileDataArgs { data: Some(data_vector) });
        file_data_vec.push(file_data);
    }
    info!("successfully built the buffer");
    let files = builder.create_vector(&file_data_vec);
    let file_data_list = FileDataList::create(&mut builder, &FileDataListArgs { files: Some(files) });
    builder.finish(file_data_list, None);
    info!("sending buffer");
    Ok(builder.finished_data().to_vec())
    }

/*//////////////////////////////////////
/// here starts delete functionality ///
////////////////////////////////////////

Handles file deletion and logging of deleted files */

struct DeleteFile {
        file: File,
    }
    
impl DeleteFile {
     /* Creates a new delete file handler for the given user
     * The handler manages a JSON file for tracking deletions
     */
    fn new(user : &str) -> Result<Self, Error> {
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(format!("{}.json", user))
                .map_err(ErrorInternalServerError)?;
            Ok(Self { file })
    }
    /* Logs the deletion of a file with its key and offset/size information
     * Creates a JSON entry with deletion details
     */
    fn delete(&mut self, key: &str,  offset_size_list: &[(u64, u64)]) -> Result<(), Error> {
            let log_entry = json!({
                key: {
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
    
/* Handles the deletion process and logs the deletion details
 * Parameters:
 * - user: User identifier
 * - key: Key of the file being deleted
 * - offset_size_list: List of offset/size pairs for the deleted content
 */
pub fn delete_and_log(user : &str,key: &str, offset_size_list: Vec<(u64, u64)>) -> Result<(), Error> {
        let mut delete_file = DeleteFile::new(&user)?;
        delete_file.delete(key, &offset_size_list)?;
    
        info!("Deleted and logged data for key: {}", key);
        Ok(())
}

/// Local XFS Binary Storage implementation
/// Uses the existing file-per-user approach with XFS filesystem
pub struct LocalXFSBinaryStore;

impl LocalXFSBinaryStore {
    pub fn new() -> Self {
        Self
    }
}

impl BinaryStorage for LocalXFSBinaryStore {
    fn write_data(&self, user_id: &str, data: &[u8]) -> Result<Vec<(u64, u64)>, Error> {
        write_files_to_storage(user_id, data)
    }

    fn read_data(&self, user_id: &str, offset_size_list: Vec<(u64, u64)>) -> Result<Vec<u8>, Error> {
        get_files_from_storage(user_id, offset_size_list)
    }

    fn log_deletion(&self, user_id: &str, key: &str, offset_size_list: Vec<(u64, u64)>) -> Result<(), Error> {
        delete_and_log(user_id, key, offset_size_list)
    }
}

/// Storage configuration enum for backend selection
#[derive(Debug, Clone)]
pub enum StorageBackend {
    LocalXFS,
    #[cfg(test)]
    Mock,
}

/// Storage manager that provides the configured storage backend
pub struct StorageManager;

impl StorageManager {
    /// Get the configured storage backend based on environment variables
    pub fn get_storage() -> Box<dyn BinaryStorage> {
        let backend = match env::var("STORAGE_BACKEND") {
            Ok(backend_str) => match backend_str.to_lowercase().as_str() {
                #[cfg(test)]
                "mock" => StorageBackend::Mock,
                _ => StorageBackend::LocalXFS,
            }
            Err(_) => StorageBackend::LocalXFS,
        };

        match backend {
            StorageBackend::LocalXFS => Box::new(LocalXFSBinaryStore::new()),
            #[cfg(test)]
            StorageBackend::Mock => Box::new(MockBinaryStorage::new()),
        }
    }
}

/// Mock Binary Storage implementation for testing
/// Stores data in memory without any disk I/O operations
#[cfg(test)]
pub struct MockBinaryStorage {
    next_offset: u64,
}

#[cfg(test)]
impl MockBinaryStorage {
    pub fn new() -> Self {
        Self {
            next_offset: 0,
        }
    }
}

#[cfg(test)]
impl BinaryStorage for MockBinaryStorage {
    fn write_data(&self, _user_id: &str, data: &[u8]) -> Result<Vec<(u64, u64)>, Error> {
        // Simulate the flatbuffer parsing and return mock offset/size pairs
        let file_data_list = match root::<FileDataList>(data) {
            Ok(data) => data,
            Err(e) => return Err(ErrorBadRequest(format!("Failed to parse FlatBuffers data: {:?}", e))),
        };
        
        let files = match file_data_list.files() {
            Some(files) => files,
            None => return Err(ErrorBadRequest("No files found in FlatBuffers data")),
        };

        let mut offset_size_list = Vec::new();
        let mut current_offset = self.next_offset;
        
        for file_data in files.iter() {
            let data = match file_data.data() {
                Some(data) => data,
                None => continue,
            };
            
            let size = data.bytes().len() as u64;
            offset_size_list.push((current_offset, size));
            current_offset += size;
        }
        
        Ok(offset_size_list)
    }

    fn read_data(&self, _user_id: &str, offset_size_list: Vec<(u64, u64)>) -> Result<Vec<u8>, Error> {
        // Create a mock flatbuffer response
        let mut builder = FlatBufferBuilder::new();
        let mut file_data_vec = Vec::new();
        
        for &(_offset, size) in offset_size_list.iter() {
            // Create mock data
            let mock_data = vec![0u8; size as usize];
            let data_vector = builder.create_vector(&mock_data);
            let file_data = FileData::create(&mut builder, &FileDataArgs { data: Some(data_vector) });
            file_data_vec.push(file_data);
        }
        
        let files = builder.create_vector(&file_data_vec);
        let file_data_list = FileDataList::create(&mut builder, &FileDataListArgs { files: Some(files) });
        builder.finish(file_data_list, None);
        
        Ok(builder.finished_data().to_vec())
    }

    fn log_deletion(&self, _user_id: &str, key: &str, _offset_size_list: Vec<(u64, u64)>) -> Result<(), Error> {
        // Mock implementation - just log the deletion
        info!("Mock: Deleted and logged data for key: {}", key);
        Ok(())
    }
}
