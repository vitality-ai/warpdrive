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
use log::{info, error, warn};
use flatbuffers::{root, FlatBufferBuilder};
use lazy_static::lazy_static;
use std::sync::Arc;

use crate::util::flatbuffer_store_generated::store::{FileDataList, FileData, FileDataArgs, FileDataListArgs};

/// Trait defining the binary storage interface
pub trait Storage: Send + Sync {
    /// Write data for a user, bucket and return (offset, size) - simulates the original append behavior
    fn write_data(&self, user_id: &str, bucket: &str, data: &[u8]) -> Result<(u64, u64), Error>;
    
    /// Read data for a user, bucket from specific offset and size
    fn read_data(&self, user_id: &str, bucket: &str, offset: u64, size: u64) -> Result<Vec<u8>, Error>;
    
    /// Log deletion for a user, bucket with key and offset/size information
    fn log_deletion(&self, user_id: &str, bucket: &str, key: &str, offset_size_list: &[(u64, u64)]) -> Result<(), Error>;
    
    /// Store binary data for an object (higher-level interface)
    fn put_object(&self, user_id: &str, object_id: &str, data: &[u8]) -> Result<(), Error>;
    
    /// Retrieve binary data for an object (higher-level interface)
    fn get_object(&self, user_id: &str, object_id: &str) -> Result<Vec<u8>, Error>;
    
    /// Delete binary data for an object (higher-level interface)
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
pub fn write_files_to_storage(context: &crate::service::user_context::UserContext, body: &[u8]) -> Result<Vec<(u64, u64)>, Error> {
    info!("write_files_to_storage called for user: {}, bucket: {}, body size: {}", context.user_id, context.bucket, body.len());
    let mut offset_size_list: Vec<(u64, u64)> = Vec::new();

    // Deserialize binary data into FileDataList using flatbuffer
    let file_data_list = match root::<FileDataList>(&body) {
        Ok(data) => {
            info!("Successfully parsed FlatBuffers data");
            data
        },
        Err(e) => {
            error!("Failed to parse FlatBuffers data: {:?}", e);
            return Err(ErrorBadRequest(format!("Failed to parse FlatBuffers data: {:?}", e)));
        },
    };
    let files = match file_data_list.files() {
        Some(files) => {
            info!("Found {} files in FlatBuffers data", files.len());
            files
        },
        None => {
            error!("No files found in FlatBuffers data");
            return Err(ErrorBadRequest("No files found in FlatBuffers data"));
        },
    };
    info!("Deserialized {} files for user: {}, bucket: {}", files.len(), context.user_id, context.bucket);

    for (index, file_data) in files.iter().enumerate() {
        let data = match file_data.data() {
            Some(data) => data,
            None => {
                error!("No data in file at index {}", index);
                continue;
            }
        };

        info!("Attempting to write file {} to storage for user: {}, bucket: {}", index, context.user_id, context.bucket);
        match STORAGE_INSTANCE.write_data(&context.user_id, &context.bucket, data.bytes()) {
            Ok((offset, size)) => {
                offset_size_list.push((offset, size));
                info!("Successfully written file {} at offset {} with size {} for user: {}, bucket: {}", 
                      index, offset, size, context.user_id, context.bucket);
            }
            Err(e) => {
                error!("Failed to write file {} to storage for user: {}, bucket: {}: {}", index, context.user_id, context.bucket, e);
                return Err(ErrorInternalServerError(e));
            }
        }
    }

    Ok(offset_size_list)
}

/// Legacy API compatibility: Retrieves and combines files from storage into a FlatBuffer
pub fn get_files_from_storage(context: &crate::service::user_context::UserContext, offset_size_list: Vec<(u64, u64)>) -> Result<Vec<u8>, Error> {
    info!("Getting files from storage using new abstraction");
    let mut builder = FlatBufferBuilder::new();
    let mut file_data_vec = Vec::new();
    info!("Building the flatbuffer to share");
    
    for &(offset, size) in offset_size_list.iter() {
        let data = STORAGE_INSTANCE.read_data(&context.user_id, &context.bucket, offset, size)
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
pub fn delete_and_log(context: &crate::service::user_context::UserContext, key: &str, offset_size_list: Vec<(u64, u64)>) -> Result<(), Error> {
    STORAGE_INSTANCE.log_deletion(&context.user_id, &context.bucket, key, &offset_size_list)?;
    info!("Deleted and logged data for key: {} in bucket: {}", key, context.bucket);
    Ok(())
}

/// S3-compatible function: Writes raw binary data to storage (not FlatBuffers)
/// Returns a vector of (offset, size) pairs for metadata storage
pub fn write_s3_data_to_storage(context: &crate::service::user_context::UserContext, data: &[u8]) -> Result<Vec<(u64, u64)>, Error> {
    info!("write_s3_data_to_storage called for user: {}, bucket: {}, data size: {}", context.user_id, context.bucket, data.len());
    
    // For S3 compatibility, we treat the raw data as a single file
    // This is different from the FlatBuffers approach which expects multiple files
    match STORAGE_INSTANCE.write_data(&context.user_id, &context.bucket, data) {
        Ok((offset, size)) => {
            info!("Successfully written S3 data at offset {} with size {} for user: {}, bucket: {}", 
                  offset, size, context.user_id, context.bucket);
            Ok(vec![(offset, size)])
        }
        Err(e) => {
            error!("Failed to write S3 data to storage for user: {}, bucket: {}: {}", context.user_id, context.bucket, e);
            Err(ErrorInternalServerError(e))
        }
    }
}

/// S3-compatible function: Retrieves raw binary data from storage (not FlatBuffers)
/// Takes offset_size_list and returns the raw data
pub fn get_s3_data_from_storage(context: &crate::service::user_context::UserContext, offset_size_list: Vec<(u64, u64)>) -> Result<Vec<u8>, Error> {
    info!("Getting S3 data from storage using new abstraction");
    
    // For S3 compatibility, we expect a single file (unlike FlatBuffers which can have multiple)
    if offset_size_list.is_empty() {
        return Err(ErrorBadRequest("No data found"));
    }
    
    if offset_size_list.len() > 1 {
        // If we have multiple chunks, concatenate them (this shouldn't happen for S3)
        warn!("S3 data has multiple chunks, concatenating them");
        let mut combined_data = Vec::new();
        for &(offset, size) in &offset_size_list {
            let chunk = STORAGE_INSTANCE.read_data(&context.user_id, &context.bucket, offset, size)
                .map_err(ErrorInternalServerError)?;
            combined_data.extend_from_slice(&chunk);
        }
        Ok(combined_data)
    } else {
        // Single chunk (normal case for S3)
        let (offset, size) = offset_size_list[0];
        let data = STORAGE_INSTANCE.read_data(&context.user_id, &context.bucket, offset, size)
            .map_err(ErrorInternalServerError)?;
        Ok(data)
    }
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