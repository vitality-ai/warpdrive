//! StorageService encapsulates business logic for interacting with the storage layer.

use actix_web::Error;
use actix_web::error::ErrorBadRequest;
use flatbuffers::{root, FlatBufferBuilder};
use crate::storage::config::StorageConfig;
use crate::service::user_context::UserContext;
use crate::service::metadata_service::MetadataService;
use crate::util::serializer::deserialize_offset_size;
use crate::util::flatbuffer_store_generated::store::{FileDataList, FileData, FileDataArgs, FileDataListArgs};

pub struct StorageService;

// Unified mode for storage IO
pub enum StorageMode { Native, S3 }

impl StorageService {
    pub fn new() -> Self { Self }

    fn store(&self) -> std::sync::Arc<dyn crate::storage::Storage> {
        StorageConfig::from_env().create_store()
    }

    // Unified write: handles Native (FlatBuffers) and S3 (raw bytes)
    pub fn write_object(&self, context: &UserContext, body: &[u8], mode: StorageMode) -> Result<Vec<(u64, u64)>, Error> {
        match mode {
            StorageMode::Native => {
                let file_data_list = root::<FileDataList>(&body)
                    .map_err(|e| ErrorBadRequest(format!("Failed to parse FlatBuffers data: {:?}", e)))?;
                let files = file_data_list.files()
                    .ok_or_else(|| ErrorBadRequest("No files found in FlatBuffers data"))?;
                let store = self.store();
                let mut out: Vec<(u64, u64)> = Vec::new();
                for file_data in files.iter() {
                    if let Some(data) = file_data.data() {
                        let (o, s) = store.write(&context.user_id, &context.bucket, data.bytes())?;
                        out.push((o, s));
                    }
                }
                Ok(out)
            }
            StorageMode::S3 => {
                let store = self.store();
                let (o, s) = store.write(&context.user_id, &context.bucket, body)?;
                Ok(vec![(o, s)])
            }
        }
    }

    // Unified read: returns FlatBuffers (Native) or raw bytes (S3)
    pub fn read_object(&self, context: &UserContext, chunks: &[(u64, u64)], mode: StorageMode) -> Result<Vec<u8>, Error> {
        match mode {
            StorageMode::Native => {
                let store = self.store();
                let mut builder = FlatBufferBuilder::new();
                let mut file_data_vec = Vec::new();
                for (offset, size) in chunks.iter().copied() {
                    let data = store.read(&context.user_id, &context.bucket, offset, size)?;
                    let data_vector = builder.create_vector(&data);
                    let file_data = FileData::create(&mut builder, &FileDataArgs { data: Some(data_vector) });
                    file_data_vec.push(file_data);
                }
                let files = builder.create_vector(&file_data_vec);
                let file_data_list = FileDataList::create(&mut builder, &FileDataListArgs { files: Some(files) });
                builder.finish(file_data_list, None);
                Ok(builder.finished_data().to_vec())
            }
            StorageMode::S3 => {
                let store = self.store();
                let mut out = Vec::new();
                for (offset, size) in chunks.iter().copied() {
                    let data = store.read(&context.user_id, &context.bucket, offset, size)?;
                    out.extend_from_slice(&data);
                }
                Ok(out)
            }
        }
    }

    // Delete an object: read metadata, delete ranges, remove metadata
    pub fn delete_object(&self, context: &UserContext, key: &str) -> Result<(), Error> {
        let db = MetadataService::new(&context.user_id)?;
        // Ensure key exists
        db.check_key_nonexistance(&context.bucket, key)?;
        // Read and deserialize ranges
        let offset_size_bytes = db.read_metadata(&context.bucket, key)
            .map_err(|_| ErrorBadRequest("Key does not exist"))?;
        let offset_size_list = deserialize_offset_size(&offset_size_bytes)?;
        // Queue deletion using the metadata service
        let db = MetadataService::new(&context.user_id)?;
        db.queue_deletion(&context.bucket, key, &offset_size_list)?;
        // Delete metadata
        db.delete_metadata(&context.bucket, key)
            .map_err(|e| actix_web::error::ErrorInternalServerError(e))
    }

    /// Delete storage chunks directly (used by deletion worker)
    pub fn delete_chunks(&self, context: &UserContext, offset_size_list: &[(u64, u64)]) -> Result<(), Error> {
        let store = self.store();
        store.delete(&context.user_id, &context.bucket, offset_size_list)
    }
}


