//! Metadata Storage Layer Abstraction

pub mod sqlite_store;
pub mod mock_store;
pub mod config;

#[cfg(test)]
mod comprehensive_test;

use actix_web::Error;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Represents the location and properties of stored data chunks
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DataChunk {
    pub offset: u64,
    pub size: u64,
}

/// Full metadata for a stored S3 object
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Metadata {
    pub chunks: Vec<DataChunk>,
    /// Legacy catch-all properties map (kept for backward compat; prefer typed fields below).
    pub properties: HashMap<String, String>,
    /// MD5 ETag in double quotes, e.g. `"d41d8cd98f00b204e9800998ecf8427e"`.
    pub etag: Option<String>,
    /// Total object size in bytes.
    pub size: u64,
    /// MIME type stored on PUT.
    pub content_type: Option<String>,
    /// RFC 2616 date string set on PUT, e.g. `"Thu, 19 Jun 2026 00:00:00 GMT"`.
    pub last_modified: Option<String>,
    /// `x-amz-meta-*` headers stored as `{"key": "value"}` (header name without prefix).
    pub user_metadata: HashMap<String, String>,
    /// `Cache-Control` header value stored on PUT.
    pub cache_control: Option<String>,
    /// `Expires` header value stored on PUT (RFC 2616 date string).
    pub expires: Option<String>,
    /// `Content-Encoding` header value stored on PUT (e.g. `"gzip"`).
    pub content_encoding: Option<String>,
    /// Version ID assigned by the store; `None` for non-versioned objects, `Some("")` for
    /// suspended-versioning null-version objects (stored as `"null"` in the DB).
    pub version_id: Option<String>,
    /// True when this row is a delete marker (no data, marks the key as deleted).
    pub is_delete_marker: bool,
    /// Checksum algorithm stored on PUT/CompleteMultipartUpload (e.g. "SHA256", "CRC32C").
    pub checksum_algorithm: Option<String>,
    /// Base64-encoded checksum value (or composite "value-N" for multipart COMPOSITE type).
    pub checksum_value: Option<String>,
    /// Checksum type: "COMPOSITE" or "FULL_OBJECT" (empty for non-checksum objects).
    pub checksum_type: Option<String>,
}

impl Metadata {
    /// Create from an offset-size list (old-API path); S3 fields default to empty/None.
    pub fn from_offset_size_list(offset_size_list: Vec<(u64, u64)>) -> Self {
        let size: u64 = offset_size_list.iter().map(|(_, s)| s).sum();
        let chunks = offset_size_list
            .into_iter()
            .map(|(offset, size)| DataChunk { offset, size })
            .collect();
        Self {
            chunks,
            properties: HashMap::new(),
            etag: None,
            size,
            content_type: None,
            last_modified: None,
            user_metadata: HashMap::new(),
            cache_control: None,
            expires: None,
            content_encoding: None,
            version_id: None,
            is_delete_marker: false,
            checksum_algorithm: None,
            checksum_value: None,
            checksum_type: None,
        }
    }

    pub fn to_offset_size_list(&self) -> Vec<(u64, u64)> {
        self.chunks.iter().map(|c| (c.offset, c.size)).collect()
    }
}

/// Per-bucket info for list-buckets
#[derive(Debug, Clone)]
pub struct BucketStats {
    pub name: String,
    pub created_at: String,
    pub object_count: u64,
    pub total_size: u64,
}

pub type ObjectId = String;
pub type UserId = String;

/// Trait defining the metadata storage interface
pub trait MetadataStorage: Send + Sync {
    fn put_metadata(&self, user_id: &str, bucket: &str, object_id: &str, metadata: &Metadata) -> Result<(), Error>;
    fn get_metadata(&self, user_id: &str, bucket: &str, object_id: &str) -> Result<Metadata, Error>;
    fn delete_metadata(&self, user_id: &str, bucket: &str, object_id: &str) -> Result<(), Error>;
    fn list_objects(&self, user_id: &str, bucket: &str) -> Result<Vec<ObjectId>, Error>;
    fn object_exists(&self, user_id: &str, bucket: &str, object_id: &str) -> Result<bool, Error>;
    fn update_metadata(&self, user_id: &str, bucket: &str, object_id: &str, metadata: &Metadata) -> Result<(), Error>;
    fn update_object_id(&self, user_id: &str, bucket: &str, old_object_id: &str, new_object_id: &str) -> Result<(), Error>;
    fn queue_deletion(&self, user_id: &str, bucket: &str, key: &str, offset_size_list: &[(u64, u64)]) -> Result<(), Error>;
    fn list_buckets_with_stats(&self, user_id: &str) -> Result<Vec<BucketStats>, Error>;

    // Bucket lifecycle
    fn create_bucket(&self, user_id: &str, bucket: &str) -> Result<(), Error>;
    fn delete_bucket(&self, user_id: &str, bucket: &str) -> Result<(), Error>;
    fn bucket_exists(&self, user_id: &str, bucket: &str) -> Result<bool, Error>;
    fn list_all_buckets_for_user(&self, user_id: &str) -> Result<Vec<String>, Error>;

    /// Returns (object_count, total_bytes) for a single bucket.
    fn bucket_object_stats(&self, user_id: &str, bucket: &str) -> Result<(u64, u64), Error>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metadata_from_offset_size_list() {
        let offset_size_list = vec![(100u64, 200u64), (300, 400)];
        let metadata = Metadata::from_offset_size_list(offset_size_list.clone());
        assert_eq!(metadata.chunks.len(), 2);
        assert_eq!(metadata.size, 600);
        assert_eq!(metadata.to_offset_size_list(), offset_size_list);
    }

    #[test]
    fn test_data_chunk_equality() {
        let chunk1 = DataChunk { offset: 100, size: 200 };
        let chunk2 = DataChunk { offset: 100, size: 200 };
        let chunk3 = DataChunk { offset: 100, size: 300 };
        assert_eq!(chunk1, chunk2);
        assert_ne!(chunk1, chunk3);
    }
}
