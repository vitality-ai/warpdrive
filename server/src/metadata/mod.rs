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

/// Result of a versioning-aware delete (no explicit versionId given).
pub enum VersioningDeleteResult {
    /// Versioning disabled — object data removed (or never existed).
    Deleted,
    /// Versioning enabled/suspended — a delete marker was created.
    Marker { version_id: String },
}

/// Result of permanently deleting a specific version.
pub struct DeleteSpecificResult {
    pub found: bool,
    pub was_delete_marker: bool,
    pub version_id: String,
}

/// One entry in a ListObjectVersions response.
pub struct VersionRow {
    pub key: String,
    pub version_id: String,
    pub is_delete_marker: bool,
    pub etag: String,
    pub size: u64,
    pub last_modified: String,
    pub is_latest: bool,
}

#[derive(Debug, Clone)]
pub struct DeletionEvent {
    pub id: i64,
    pub user_id: String,
    pub bucket: String,
    pub key: String,
    pub offset_size_list: Vec<(u64, u64)>,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct MultipartUploadRow {
    pub upload_id: String,
    pub user_id: String,
    pub bucket: String,
    pub key: String,
    pub content_type: Option<String>,
    pub metadata_json: String,
    pub initiated_at: String,
    pub status: String,
    pub final_etag: Option<String>,
    pub checksum_algorithm: String,
    pub checksum_type: String,
    pub object_lock_mode: String,
    pub object_lock_retain_until: String,
    pub object_lock_legal_hold: String,
}

#[derive(Debug, Clone)]
pub struct MultipartPartRow {
    pub part_number: i32,
    pub etag: String,
    pub size: u64,
    pub extents_blob: Vec<u8>,
    pub checksum_value: String,
}

#[derive(Debug, Clone)]
pub struct ObjectLockRow {
    pub mode: Option<String>,
    pub retain_until_date: Option<String>,
    pub legal_hold: String,
}

/// Trait defining the metadata storage interface.
///
/// Single implementation today is `sqlite_store::SQLiteMetadataStore`
/// (plus `mock_store::MockMetadataStore` for tests) — every method here is
/// something `service/metadata_service.rs` actually calls, not a
/// speculative superset, so a future second backend (e.g. a
/// quorum-replicated cluster metadata store) implements exactly this and
/// nothing in `service/metadata_service.rs` or any S3 handler needs to
/// change.
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

    // --- Full-metadata S3 PUT path ---
    /// Returns (version_id, old_extents_to_gc).
    fn put_object_v2(&self, user_id: &str, bucket: &str, key: &str, metadata: &Metadata) -> Result<(Option<String>, Vec<(u64, u64)>), Error>;

    // --- Deletion WAL ---
    fn get_pending_deletions(&self, limit: i32) -> Result<Vec<DeletionEvent>, Error>;
    fn mark_deletion_processed(&self, id: i64) -> Result<(), Error>;
    fn cleanup_old_deletions(&self) -> Result<usize, Error>;

    // --- CORS ---
    fn set_bucket_cors(&self, bucket: &str, cors_xml: &str) -> Result<(), Error>;
    fn get_bucket_cors(&self, bucket: &str) -> Result<Option<String>, Error>;
    fn delete_bucket_cors(&self, bucket: &str) -> Result<(), Error>;

    // --- Bucket location ---
    fn set_bucket_location(&self, user_id: &str, bucket: &str, location: &str) -> Result<(), Error>;
    fn get_bucket_location(&self, user_id: &str, bucket: &str) -> Result<String, Error>;

    // --- Versioning ---
    fn get_versioning_state(&self, bucket: &str) -> Result<String, Error>;
    fn set_versioning_state(&self, user_id: &str, bucket: &str, state: &str) -> Result<(), Error>;
    fn delete_object_v2(&self, user_id: &str, bucket: &str, key: &str) -> Result<VersioningDeleteResult, Error>;
    fn delete_specific_version(&self, user_id: &str, bucket: &str, key: &str, version_id: &str) -> Result<DeleteSpecificResult, Error>;
    fn get_object_version(&self, user_id: &str, bucket: &str, key: &str, version_id: &str) -> Result<Metadata, Error>;
    /// Returns the last_modified of the is_latest=1 row, including delete markers.
    fn get_latest_last_modified(&self, user_id: &str, bucket: &str, key: &str) -> Result<Option<String>, Error>;
    fn list_object_versions_full(
        &self, user_id: &str, bucket: &str,
        prefix: &str, key_marker: &str, version_id_marker: &str, max_keys: usize,
    ) -> Result<(Vec<VersionRow>, bool, String, String), Error>;

    // --- Tagging ---
    fn set_object_tags(&self, user_id: &str, bucket: &str, key: &str, tags: &[(String, String)]) -> Result<(), Error>;
    fn get_object_tags(&self, user_id: &str, bucket: &str, key: &str) -> Result<Vec<(String, String)>, Error>;
    fn delete_object_tags(&self, user_id: &str, bucket: &str, key: &str) -> Result<(), Error>;
    fn get_object_tag_count(&self, user_id: &str, bucket: &str, key: &str) -> Result<i64, Error>;
    fn set_bucket_tags(&self, bucket: &str, tags: &[(String, String)]) -> Result<(), Error>;
    fn get_bucket_tags(&self, bucket: &str) -> Result<Vec<(String, String)>, Error>;
    fn delete_bucket_tags(&self, bucket: &str) -> Result<(), Error>;
    fn set_multipart_tagging(&self, upload_id: &str, tagging: &str) -> Result<(), Error>;
    fn get_multipart_tagging(&self, upload_id: &str) -> Result<String, Error>;

    // --- Multipart upload management ---
    fn create_multipart_upload(
        &self, upload_id: &str, user_id: &str, bucket: &str, key: &str,
        content_type: Option<&str>, metadata_json: &str, initiated_at: &str,
        checksum_algorithm: &str, checksum_type: &str,
        object_lock_mode: &str, object_lock_retain_until: &str, object_lock_legal_hold: &str,
    ) -> Result<(), Error>;
    fn get_multipart_upload(&self, upload_id: &str) -> Result<Option<MultipartUploadRow>, Error>;
    fn mark_multipart_completed(&self, upload_id: &str, final_etag: &str) -> Result<(), Error>;
    fn delete_multipart_upload(&self, upload_id: &str) -> Result<(), Error>;
    fn delete_completed_uploads_for_key(&self, bucket: &str, key: &str) -> Result<(), Error>;
    fn list_bucket_multipart_uploads(&self, bucket: &str) -> Result<Vec<MultipartUploadRow>, Error>;
    fn upsert_multipart_part(
        &self, upload_id: &str, part_number: i32, etag: &str, size: u64, extents_blob: &[u8],
        checksum_value: &str,
    ) -> Result<(), Error>;
    fn list_multipart_parts(&self, upload_id: &str) -> Result<Vec<MultipartPartRow>, Error>;
    fn delete_parts_for_upload(&self, upload_id: &str) -> Result<(), Error>;
    fn get_parts_manifest(&self, user_id: &str, bucket: &str, key: &str) -> Result<Option<String>, Error>;
    fn set_parts_manifest(&self, user_id: &str, bucket: &str, key: &str, manifest: &str) -> Result<(), Error>;

    // --- Object Lock ---
    fn get_bucket_object_lock_enabled(&self, bucket: &str) -> Result<bool, Error>;
    fn set_bucket_object_lock_enabled(&self, bucket: &str, enabled: bool) -> Result<(), Error>;
    fn create_bucket_with_lock(&self, user_id: &str, bucket: &str, lock_enabled: bool) -> Result<(), Error>;
    fn get_object_lock_config(&self, bucket: &str) -> Result<Option<(String, Option<i64>, Option<i64>)>, Error>;
    fn put_object_lock_config(&self, bucket: &str, mode: &str, days: Option<i64>, years: Option<i64>) -> Result<(), Error>;
    fn get_object_lock(&self, bucket: &str, key: &str, version_id: &str) -> Result<Option<ObjectLockRow>, Error>;
    fn put_object_lock(
        &self, bucket: &str, key: &str, version_id: &str,
        mode: Option<&str>, retain_until_date: Option<&str>, legal_hold: Option<&str>,
    ) -> Result<(), Error>;
    fn set_object_legal_hold(&self, bucket: &str, key: &str, version_id: &str, status: &str) -> Result<(), Error>;
    /// Returns (blocked_by_retention, blocked_by_legal_hold).
    fn check_object_lock_protection(
        &self, bucket: &str, key: &str, version_id: &str, bypass_governance: bool,
    ) -> Result<(bool, bool), Error>;
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
