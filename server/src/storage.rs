// storage.rs

// Legacy imports - still needed for potential backwards compatibility
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
use crate::binary::{BinaryStorage, config::BinaryConfig};

// Global binary storage instance - initialized once for the application
lazy_static::lazy_static! {
    static ref BINARY_STORAGE: std::sync::Arc<dyn BinaryStorage> = {
        let config = BinaryConfig::from_env();
        config.create_store()
    };
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

//////////////////////////////////////////
/// Legacy implementation - now replaced by binary storage abstraction ///
/// Kept for reference and potential backwards compatibility ///
//////////////////////////////////////////

/* OpenFile provides operations for interacting with binary (.bin) files:
 * - Creating new files
 * - Reading existing files
 * - Writing data to files
 * - Managing file seek operations
 */
#[allow(dead_code)]
struct OpenFile {
    file: File,
}

#[allow(dead_code)]
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

    // Extract data from flatbuffer for batch processing
    let mut data_list = Vec::new();
    for (index, file_data) in files.iter().enumerate() {
        let data = match file_data.data() {
            Some(data) => data,
            None => {
                error!("No data in file at index {}", index);
                continue;
            }
        };
        data_list.push(data.bytes());
    }

    // Use binary storage abstraction for batch write
    BINARY_STORAGE.put_objects_batch(user, data_list)
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
    info!("Getting files from binary storage");
    
    // Use binary storage abstraction for batch read
    let data_list = BINARY_STORAGE.get_objects_batch(user, &offset_size_list)?;
    
    info!("Retrieved {} files, building the flatbuffer to share", data_list.len());
    let mut builder = FlatBufferBuilder::new();
    let mut file_data_vec = Vec::new();
    
    for data in data_list {
        let data_vector = builder.create_vector(&data);
        let file_data = FileData::create(&mut builder, &FileDataArgs { data: Some(data_vector) });
        file_data_vec.push(file_data);
    }
    
    info!("Successfully built the buffer");
    let files = builder.create_vector(&file_data_vec);
    let file_data_list = FileDataList::create(&mut builder, &FileDataListArgs { files: Some(files) });
    builder.finish(file_data_list, None);
    info!("Sending buffer");
    Ok(builder.finished_data().to_vec())
}

//////////////////////////////////////////
/// Legacy delete functionality - now handled by binary storage abstraction ///
//////////////////////////////////////////

// Handles file deletion and logging of deleted files

#[allow(dead_code)]
struct DeleteFile {
        file: File,
    }
    
#[allow(dead_code)]
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
    // Use binary storage abstraction for deletion
    BINARY_STORAGE.delete_object(user, key, &offset_size_list)
}
