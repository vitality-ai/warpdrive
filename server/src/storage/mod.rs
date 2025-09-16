//! Binary Storage Layer Abstraction
//! 
//! This module provides an abstraction over binary storage backends,
//! allowing the system to use different storage implementations (local files,
//! distributed file systems, etc.) without affecting higher-level services.

pub mod local_store;
pub mod mock_store;
pub mod config;

#[cfg(test)]
mod comprehensive_test;

use actix_web::Error;
use actix_web::error::{ErrorInternalServerError, ErrorBadRequest};
use log::{info, error};
use flatbuffers::{root, FlatBufferBuilder};
use lazy_static::lazy_static;
use std::sync::Arc;

use crate::util::flatbuffer_store_generated::store::{FileDataList, FileData, FileDataArgs, FileDataListArgs};

/// Trait defining the binary storage interface
pub trait Storage: Send + Sync {
    /// Store binary data for an object
    fn put_object(&self, user_id: &str, object_id: &str, data: &[u8]) -> Result<(), Error>;
    
    /// Retrieve binary data for an object
    fn get_object(&self, user_id: &str, object_id: &str) -> Result<Vec<u8>, Error>;
    
    /// Delete binary data for an object
    fn delete_object(&self, user_id: &str, object_id: &str) -> Result<(), Error>;
    
    /// Verify object integrity using checksum
    fn verify_object(&self, user_id: &str, object_id: &str, checksum: &[u8]) -> Result<bool, Error>;
}

/// Object identifier type
pub type ObjectId = String;

/// User identifier type 
pub type UserId = String;

lazy_static! {
    static ref STORAGE_INSTANCE: Arc<dyn Storage> = {
        let config = config::StorageConfig::from_env();
        config.create_store()
    };
}

/// Legacy API compatibility: Processes incoming flatbuffer data and writes files to storage
/// Returns a vector of (offset, size) pairs for each written file
pub fn write_files_to_storage(user: &str, body: &[u8]) -> Result<Vec<(u64, u64)>, Error> {
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

        let object_id = format!("file_{}", index);
        match STORAGE_INSTANCE.put_object(user, &object_id, data.bytes()) {
            Ok(()) => {
                // For compatibility, we need to simulate offset/size
                // In the new abstraction, we don't expose internal offset/size details
                // But we return dummy values for now to maintain API compatibility
                let offset = index as u64 * 1000; // Dummy offset
                let size = data.bytes().len() as u64;
                offset_size_list.push((offset, size));
                info!("Written file {} as object {} with size {}", index, object_id, size);
            }
            Err(e) => {
                error!("Failed to write file {} to storage: {}", index, e);
                return Err(ErrorInternalServerError(e));
            }
        }
    }

    Ok(offset_size_list)
}

/// Legacy API compatibility: Retrieves and combines files from storage into a FlatBuffer
pub fn get_files_from_storage(user: &str, offset_size_list: Vec<(u64, u64)>) -> Result<Vec<u8>, Error> {
    info!("Getting files from storage using new abstraction");
    let mut builder = FlatBufferBuilder::new();
    let mut file_data_vec = Vec::new();
    info!("Building the flatbuffer to share");
    
    for (index, &(_offset, _size)) in offset_size_list.iter().enumerate() {
        let object_id = format!("file_{}", index);
        let data = STORAGE_INSTANCE.get_object(user, &object_id)
            .map_err(ErrorInternalServerError)?;
        
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

/// Legacy API compatibility: Handles the deletion process and logs the deletion details
pub fn delete_and_log(user: &str, key: &str, offset_size_list: Vec<(u64, u64)>) -> Result<(), Error> {
    for (index, &(_offset, _size)) in offset_size_list.iter().enumerate() {
        let object_id = format!("file_{}", index);
        STORAGE_INSTANCE.delete_object(user, &object_id)?;
    }
    
    info!("Deleted and logged data for key: {}", key);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_object_and_user_id_types() {
        let user_id: UserId = "test_user".to_string();
        let object_id: ObjectId = "test_object".to_string();
        
        assert_eq!(user_id, "test_user");
        assert_eq!(object_id, "test_object");
    }
}