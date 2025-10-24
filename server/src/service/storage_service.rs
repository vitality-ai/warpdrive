//! Storage service layer that provides a clean interface to the storage abstraction

use crate::storage::Storage;
use crate::service::user_context::UserContext;
use std::sync::Arc;
use actix_web::Error;
use actix_web::error::{ErrorInternalServerError, ErrorBadRequest};
use log::{info, error, warn};
use flatbuffers::{root, FlatBufferBuilder};
use md5;

use crate::util::flatbuffer_store_generated::store::{FileDataList, FileData, FileDataArgs, FileDataListArgs};

/// Storage service that provides a clean interface to the storage abstraction
pub struct StorageService {
    storage: Arc<dyn Storage>,
}

impl StorageService {
    /// Create a new storage service with injected storage backend
    pub fn new(storage: Arc<dyn Storage>) -> Self {
        Self { storage }
    }
    
    /// Write data to storage and return (offset, size)
    pub fn write_data(&self, user_id: &str, bucket: &str, data: &[u8]) -> Result<(u64, u64), Error> {
        self.storage.write_data(user_id, bucket, data)
    }
    
    /// Read data from storage at specific offset and size
    pub fn read_data(&self, user_id: &str, bucket: &str, offset: u64, size: u64) -> Result<Vec<u8>, Error> {
        self.storage.read_data(user_id, bucket, offset, size)
    }
    
    
    /// Verify object integrity using checksum
    pub fn verify_object(&self, user_id: &str, bucket: &str, key: &str, checksum: &[u8]) -> Result<bool, Error> {
        self.storage.verify_object(user_id, bucket, key, checksum)
    }

    /// Calculate MD5 checksum for data
    fn calculate_checksum(data: &[u8]) -> String {
        let hash = md5::compute(data);
        format!("{:x}", hash)
    }

    /// Unified storage function: Processes incoming data and writes files to storage
    /// Returns a vector of (offset, size) pairs for each written file
    /// 
    /// # Arguments
    /// * `context` - User context for storage operations
    /// * `body` - Raw data to store
    /// * `is_s3_compatible` - If true, treats body as raw S3 data; if false, parses as FlatBuffers
    pub fn write_files_to_storage(&self, context: &UserContext, body: &[u8], is_s3_compatible: bool) -> Result<Vec<(u64, u64)>, Error> {
        if is_s3_compatible {
            // S3 compatibility: treat raw data as a single file
            match self.write_data(&context.user_id, &context.bucket, body) {
                Ok((offset, size)) => Ok(vec![(offset, size)]),
                Err(e) => {
                    error!("Failed to write S3 data to storage for user: {}, bucket: {}: {}", context.user_id, context.bucket, e);
                    Err(ErrorInternalServerError(e))
                }
            }
        } else {
            // Legacy FlatBuffers compatibility: parse and process multiple files
            let mut offset_size_list: Vec<(u64, u64)> = Vec::new();

            // Deserialize binary data into FileDataList using flatbuffer
            let file_data_list = match root::<FileDataList>(&body) {
                Ok(data) => data,
                Err(e) => {
                    error!("Failed to parse FlatBuffers data: {:?}", e);
                    return Err(ErrorBadRequest(format!("Failed to parse FlatBuffers data: {:?}", e)));
                },
            };
            let files = match file_data_list.files() {
                Some(files) => files,
                None => {
                    error!("No files found in FlatBuffers data");
                    return Err(ErrorBadRequest("No files found in FlatBuffers data"));
                },
            };

            for (index, file_data) in files.iter().enumerate() {
                let data = match file_data.data() {
                    Some(data) => data,
                    None => {
                        error!("No data in file at index {}", index);
                        continue;
                    }
                };

                match self.write_data(&context.user_id, &context.bucket, data.bytes()) {
                    Ok((offset, size)) => {
                        offset_size_list.push((offset, size));
                    }
                    Err(e) => {
                        error!("Failed to write file {} to storage for user: {}, bucket: {}: {}", index, context.user_id, context.bucket, e);
                        return Err(ErrorInternalServerError(e));
                    }
                }
            }

            Ok(offset_size_list)
        }
    }

    /// Unified function: Retrieves files from storage
    /// 
    /// # Arguments
    /// * `context` - User context for storage operations
    /// * `offset_size_list` - List of (offset, size) pairs to retrieve
    /// * `is_s3_compatible` - If true, returns raw data; if false, returns FlatBuffer
    pub fn get_files_from_storage(&self, context: &UserContext, offset_size_list: Vec<(u64, u64)>, is_s3_compatible: bool) -> Result<Vec<u8>, Error> {
        if is_s3_compatible {
            // S3 compatibility: return raw data
            if offset_size_list.is_empty() {
                return Err(ErrorBadRequest("No data found"));
            }
            
            if offset_size_list.len() > 1 {
                // If we have multiple chunks, concatenate them (this shouldn't happen for S3)
                warn!("S3 data has multiple chunks, concatenating them");
                let mut combined_data = Vec::new();
                for &(offset, size) in &offset_size_list {
                    let chunk = self.read_data(&context.user_id, &context.bucket, offset, size)
                        .map_err(ErrorInternalServerError)?;
                    combined_data.extend_from_slice(&chunk);
                }
                Ok(combined_data)
            } else {
                // Single chunk (normal case for S3)
                let (offset, size) = offset_size_list[0];
                let data = self.read_data(&context.user_id, &context.bucket, offset, size)
                    .map_err(ErrorInternalServerError)?;
                Ok(data)
            }
        } else {
            // Legacy FlatBuffers compatibility: build FlatBuffer
            let mut builder = FlatBufferBuilder::new();
            let mut file_data_vec = Vec::new();
            
            for &(offset, size) in offset_size_list.iter() {
                let data = self.read_data(&context.user_id, &context.bucket, offset, size)
                    .map_err(ErrorInternalServerError)?;
                
                let data_vector = builder.create_vector(&data);
                let file_data = FileData::create(&mut builder, &FileDataArgs { data: Some(data_vector) });
                file_data_vec.push(file_data);
            }
            
            let files = builder.create_vector(&file_data_vec);
            let file_data_list = FileDataList::create(&mut builder, &FileDataListArgs { files: Some(files) });
            builder.finish(file_data_list, None);
            Ok(builder.finished_data().to_vec())
        }
    }

    /// Delete an object from storage
    pub fn delete_object(&self, context: &UserContext, key: &str, offset_size_list: &[(u64, u64)]) -> Result<(), Error> {
        self.storage.delete_object(&context.user_id, &context.bucket, key, offset_size_list)
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_storage_service_creation() {
        use crate::storage::mock_store::MockBinaryStore;
        use std::sync::Arc;
        let storage = Arc::new(MockBinaryStore::new());
        let service = StorageService::new(storage);
        assert!(true); // Service creation should not panic
    }
}
