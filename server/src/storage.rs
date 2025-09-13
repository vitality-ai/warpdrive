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
use std::collections::HashMap;
use std::sync::Mutex;

use crate::util::flatbuffer_store_generated::store::{FileDataList, FileData, FileDataArgs, FileDataListArgs};

/// Trait defining the interface for binary storage operations
/// 
/// This trait abstracts the binary storage layer, allowing different storage backends
/// to be used without changing the higher-level service logic.
/// 
/// ## Available Implementations
/// - `LocalXFSBinaryStore`: Local file system storage (default)
/// - `MockBinaryStore`: In-memory storage for testing
/// 
/// ## Configuration
/// The storage backend can be configured using the `STORAGE_BACKEND` environment variable:
/// - `"local"`, `"xfs"`, `"localxfs"`: Use local XFS file system storage
/// - `"mock"`: Use in-memory mock storage (for testing)
/// 
/// Example:
/// ```bash
/// export STORAGE_BACKEND=local
/// cargo run
/// ```
pub trait BinaryStorage: Send + Sync {
    /// Write files from flatbuffer data and return list of (offset, size) pairs
    fn put_files(&self, user_id: &str, data: &[u8]) -> Result<Vec<(u64, u64)>, Error>;
    
    /// Get files from storage using offset/size pairs and return flatbuffer data
    fn get_files(&self, user_id: &str, offset_size_list: Vec<(u64, u64)>) -> Result<Vec<u8>, Error>;
    
    /// Delete files and log the deletion
    fn delete_files(&self, user_id: &str, key: &str, offset_size_list: Vec<(u64, u64)>) -> Result<(), Error>;
}


/// Storage backend configuration
#[derive(Debug, Clone, PartialEq)]
pub enum StorageBackend {
    LocalXFS,
    Mock,
}

impl std::str::FromStr for StorageBackend {
    type Err = String;
    
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "local" | "xfs" | "localxfs" => Ok(StorageBackend::LocalXFS),
            "mock" => Ok(StorageBackend::Mock),
            _ => Err(format!("Unknown storage backend: {}", s)),
        }
    }
}

/// Create a storage instance based on configuration
pub fn create_storage_backend(backend: StorageBackend) -> Box<dyn BinaryStorage> {
    match backend {
        StorageBackend::LocalXFS => Box::new(LocalXFSBinaryStore::new()),
        StorageBackend::Mock => Box::new(MockBinaryStore::new()),
    }
}

/// Get storage backend from environment variable or use default
pub fn get_configured_storage_backend() -> Box<dyn BinaryStorage> {
    let backend = env::var("STORAGE_BACKEND")
        .unwrap_or_else(|_| "local".to_string())
        .parse()
        .unwrap_or_else(|e| {
            warn!("Invalid storage backend configuration: {}. Using default LocalXFS", e);
            StorageBackend::LocalXFS
        });
    
    info!("Using storage backend: {:?}", backend);
    create_storage_backend(backend)
}

/// Mock binary storage implementation for testing
pub struct MockBinaryStore {
    // Storage maps user_id -> list of stored data chunks
    storage: Mutex<HashMap<String, Vec<Vec<u8>>>>,
    // Track deletions: user_id -> list of (key, offset_size_list) pairs
    deletions: Mutex<HashMap<String, Vec<(String, Vec<(u64, u64)>)>>>,
}

impl MockBinaryStore {
    pub fn new() -> Self {
        Self {
            storage: Mutex::new(HashMap::new()),
            deletions: Mutex::new(HashMap::new()),
        }
    }
    
    pub fn get_stored_data(&self, user_id: &str) -> Vec<Vec<u8>> {
        let storage = self.storage.lock().unwrap();
        storage.get(user_id).cloned().unwrap_or_default()
    }
    
    pub fn get_deletions(&self, user_id: &str) -> Vec<(String, Vec<(u64, u64)>)> {
        let deletions = self.deletions.lock().unwrap();
        deletions.get(user_id).cloned().unwrap_or_default()
    }
}

impl BinaryStorage for MockBinaryStore {
    fn put_files(&self, user_id: &str, data: &[u8]) -> Result<Vec<(u64, u64)>, Error> {
        // Parse the flatbuffer data to extract individual files
        let file_data_list = match root::<FileDataList>(data) {
            Ok(data) => data,
            Err(e) => return Err(ErrorBadRequest(format!("Failed to parse FlatBuffers data: {:?}", e))),
        };
        
        let files = match file_data_list.files() {
            Some(files) => files,
            None => return Err(ErrorBadRequest("No files found in FlatBuffers data")),
        };
        
        let mut storage = self.storage.lock().unwrap();
        let user_storage = storage.entry(user_id.to_string()).or_insert_with(Vec::new);
        
        let mut offset_size_list = Vec::new();
        
        for file_data in files.iter() {
            let data = match file_data.data() {
                Some(data) => data.bytes().to_vec(),
                None => continue,
            };
            
            let offset = user_storage.len() as u64;
            let size = data.len() as u64;
            user_storage.push(data);
            offset_size_list.push((offset, size));
        }
        
        info!("MockBinaryStore: Stored {} files for user {}", offset_size_list.len(), user_id);
        Ok(offset_size_list)
    }
    
    fn get_files(&self, user_id: &str, offset_size_list: Vec<(u64, u64)>) -> Result<Vec<u8>, Error> {
        let storage = self.storage.lock().unwrap();
        let user_storage = storage.get(user_id).ok_or_else(|| {
            ErrorBadRequest(format!("No storage found for user: {}", user_id))
        })?;
        
        let mut builder = FlatBufferBuilder::new();
        let mut file_data_vec = Vec::new();
        
        for &(offset, _size) in offset_size_list.iter() {
            let data = user_storage.get(offset as usize).ok_or_else(|| {
                ErrorBadRequest(format!("Invalid offset {} for user {}", offset, user_id))
            })?;
            
            let data_vector = builder.create_vector(data);
            let file_data = FileData::create(&mut builder, &FileDataArgs { data: Some(data_vector) });
            file_data_vec.push(file_data);
        }
        
        let files = builder.create_vector(&file_data_vec);
        let file_data_list = FileDataList::create(&mut builder, &FileDataListArgs { files: Some(files) });
        builder.finish(file_data_list, None);
        
        info!("MockBinaryStore: Retrieved {} files for user {}", offset_size_list.len(), user_id);
        Ok(builder.finished_data().to_vec())
    }
    
    fn delete_files(&self, user_id: &str, key: &str, offset_size_list: Vec<(u64, u64)>) -> Result<(), Error> {
        let mut deletions = self.deletions.lock().unwrap();
        let user_deletions = deletions.entry(user_id.to_string()).or_insert_with(Vec::new);
        user_deletions.push((key.to_string(), offset_size_list));
        
        info!("MockBinaryStore: Logged deletion for user {} key {}", user_id, key);
        Ok(())
    }
}

/// Local XFS-based binary storage implementation
pub struct LocalXFSBinaryStore;

impl LocalXFSBinaryStore {
    pub fn new() -> Self {
        Self
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
}

impl BinaryStorage for LocalXFSBinaryStore {
    fn put_files(&self, user_id: &str, data: &[u8]) -> Result<Vec<(u64, u64)>, Error> {
        write_files_to_storage(user_id, data)
    }
    
    fn get_files(&self, user_id: &str, offset_size_list: Vec<(u64, u64)>) -> Result<Vec<u8>, Error> {
        get_files_from_storage(user_id, offset_size_list)
    }
    
    fn delete_files(&self, user_id: &str, key: &str, offset_size_list: Vec<(u64, u64)>) -> Result<(), Error> {
        delete_and_log(user_id, key, offset_size_list)
    }
}

fn get_storage_directory() -> PathBuf {
    LocalXFSBinaryStore::get_storage_directory()
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

#[cfg(test)]
mod tests {
    use super::*;
    use flatbuffers::FlatBufferBuilder;

    fn create_test_flatbuffer_data() -> Vec<u8> {
        let mut builder = FlatBufferBuilder::new();
        
        // Create test data
        let test_data1 = vec![1u8, 2, 3, 4, 5];
        let test_data2 = vec![10u8, 20, 30];
        
        let data1_vector = builder.create_vector(&test_data1);
        let data2_vector = builder.create_vector(&test_data2);
        
        let file1 = FileData::create(&mut builder, &FileDataArgs { data: Some(data1_vector) });
        let file2 = FileData::create(&mut builder, &FileDataArgs { data: Some(data2_vector) });
        
        let files_vec = vec![file1, file2];
        let files = builder.create_vector(&files_vec);
        
        let file_data_list = FileDataList::create(&mut builder, &FileDataListArgs { files: Some(files) });
        builder.finish(file_data_list, None);
        
        builder.finished_data().to_vec()
    }

    #[test]
    fn test_storage_backend_enum_from_str() {
        assert_eq!("local".parse::<StorageBackend>().unwrap(), StorageBackend::LocalXFS);
        assert_eq!("xfs".parse::<StorageBackend>().unwrap(), StorageBackend::LocalXFS);
        assert_eq!("localxfs".parse::<StorageBackend>().unwrap(), StorageBackend::LocalXFS);
        assert_eq!("mock".parse::<StorageBackend>().unwrap(), StorageBackend::Mock);
        
        assert!("invalid".parse::<StorageBackend>().is_err());
    }

    #[test]
    fn test_create_storage_backend() {
        let _local_storage = create_storage_backend(StorageBackend::LocalXFS);
        // We can't easily test the concrete type due to trait objects,
        // but we can ensure it's created without panicking
        assert!(true);

        let _mock_storage = create_storage_backend(StorageBackend::Mock);
        assert!(true);
    }

    #[test]
    fn test_mock_binary_store_basic_operations() {
        let mock_store = MockBinaryStore::new();
        let user_id = "test_user";
        
        // Test put_files
        let test_data = create_test_flatbuffer_data();
        let offset_size_list = mock_store.put_files(user_id, &test_data).unwrap();
        
        assert_eq!(offset_size_list.len(), 2); // We created 2 files
        assert_eq!(offset_size_list[0], (0, 5)); // First file: offset 0, size 5
        assert_eq!(offset_size_list[1], (1, 3)); // Second file: offset 1, size 3
        
        // Test get_files
        let retrieved_data = mock_store.get_files(user_id, offset_size_list.clone()).unwrap();
        assert!(!retrieved_data.is_empty());
        
        // Test delete_files
        let key = "test_key";
        assert!(mock_store.delete_files(user_id, key, offset_size_list.clone()).is_ok());
        
        // Verify deletion was logged
        let deletions = mock_store.get_deletions(user_id);
        assert_eq!(deletions.len(), 1);
        assert_eq!(deletions[0].0, key);
        assert_eq!(deletions[0].1, offset_size_list);
    }

    #[test]
    fn test_mock_binary_store_error_cases() {
        let mock_store = MockBinaryStore::new();
        
        // Test with invalid flatbuffer data
        let invalid_data = vec![1u8, 2, 3];
        let result = mock_store.put_files("user", &invalid_data);
        assert!(result.is_err());
        
        // Test get_files with non-existent user
        let result = mock_store.get_files("non_existent_user", vec![(0, 1)]);
        assert!(result.is_err());
        
        // Test get_files with invalid offset
        let mock_store = MockBinaryStore::new();
        let test_data = create_test_flatbuffer_data();
        let _ = mock_store.put_files("user", &test_data).unwrap();
        
        let result = mock_store.get_files("user", vec![(999, 1)]);
        assert!(result.is_err());
    }

    #[test]
    fn test_binary_storage_trait_local_xfs() {
        let storage: Box<dyn BinaryStorage> = Box::new(LocalXFSBinaryStore::new());
        let user_id = "test_user_trait";
        
        // Test that the trait methods are callable
        let test_data = create_test_flatbuffer_data();
        let offset_size_list = storage.put_files(user_id, &test_data).unwrap();
        
        assert!(!offset_size_list.is_empty());
        
        let retrieved_data = storage.get_files(user_id, offset_size_list.clone()).unwrap();
        assert!(!retrieved_data.is_empty());
        
        let key = "test_key_trait";
        assert!(storage.delete_files(user_id, key, offset_size_list).is_ok());
    }

    #[test]
    fn test_multiple_users_isolation() {
        let mock_store = MockBinaryStore::new();
        let test_data = create_test_flatbuffer_data();
        
        // Store data for user1
        let offset_size_list1 = mock_store.put_files("user1", &test_data).unwrap();
        
        // Store data for user2
        let offset_size_list2 = mock_store.put_files("user2", &test_data).unwrap();
        
        // Both users should have their own storage
        assert_eq!(offset_size_list1, offset_size_list2); // Same offsets because separate storage
        
        // Verify user1 can retrieve their data
        let data1 = mock_store.get_files("user1", offset_size_list1).unwrap();
        assert!(!data1.is_empty());
        
        // Verify user2 can retrieve their data
        let data2 = mock_store.get_files("user2", offset_size_list2).unwrap();
        assert!(!data2.is_empty());
        
        // Verify users can't access each other's data at wrong offsets
        let result = mock_store.get_files("user1", vec![(999, 1)]);
        assert!(result.is_err());
    }
}
