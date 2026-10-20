// Author: Tejus Chandrashekar
//! `MetadataStorage` implementation backed by the cluster's own
//! `LocationStore`/`ContentLocationStore` -- what lets `MetadataService`
//! (and anything built on it) read cluster-placed objects' metadata the
//! same way it reads single-node's SQLite-backed metadata.
//!
//! **Read-only by design, not an oversight.** `MetadataStorage` models
//! `Metadata` (etag, content-type, chunks, ...) on the assumption bytes
//! were already written somewhere else before metadata is persisted --
//! true for single-node (`Storage::write` happens first, then
//! `MetadataService::put_object_full`). Cluster has no such split: an
//! object's bytes, its erasure-coded placement, and its metadata are all
//! decided and written together by `coordinator::cluster_put_object_inner`
//! (EC encode, placement, shard fan-out, quorum, replication -- none of
//! which this trait has any way to express). So every write-shaped method
//! here (`put_metadata`, `put_object_v2`, `update_metadata`,
//! `update_object_id`) returns a clear error instead of trying to fake a
//! placement decision this store has no business making; real cluster
//! writes go through `cluster_put_object_inner` directly, never through
//! this trait. What this store is genuinely good for -- and the only
//! reason it exists -- is metadata *reads*: `get_metadata`,
//! `object_exists`, `list_objects`, bucket-level stats.

use actix_web::error::{ErrorInternalServerError, ErrorNotFound};
use actix_web::Error;
use std::sync::Arc;

use super::content_location_store::ContentLocationStore;
use super::location_store::LocationStore;
use crate::metadata::{
    BucketStats, DeleteSpecificResult, DeletionEvent, Metadata, MetadataStorage,
    MultipartPartRow, MultipartUploadRow, ObjectId, ObjectLockRow, VersionRow,
    VersioningDeleteResult,
};

/// One clear error for every method that would need to create or mutate
/// a placement record outside `cluster_put_object_inner`'s EC/placement
/// machinery -- see this module's own doc for why that's not meaningful.
fn not_supported(what: &str) -> Error {
    ErrorInternalServerError(format!(
        "{what} is not supported by the cluster metadata backend; object writes go through \
         the cluster object-write path (coordinator::cluster_put_object_inner), not MetadataStorage"
    ))
}

pub struct ClusterMetadataStore {
    location_store: Arc<dyn LocationStore>,
    content_location_store: Arc<dyn ContentLocationStore>,
}

impl ClusterMetadataStore {
    pub fn new(
        location_store: Arc<dyn LocationStore>,
        content_location_store: Arc<dyn ContentLocationStore>,
    ) -> Self {
        Self { location_store, content_location_store }
    }

    fn not_found(bucket: &str, object_id: &str) -> Error {
        ErrorNotFound(format!(
            "No data found for key: {} in bucket: {}, The key does not exist", object_id, bucket
        ))
    }
}

impl MetadataStorage for ClusterMetadataStore {
    fn put_metadata(&self, _user_id: &str, _bucket: &str, _object_id: &str, _metadata: &Metadata) -> Result<(), Error> {
        Err(not_supported("put_metadata"))
    }

    // Content-dependent (packed) checked first, same precedence
    // `cluster_get_object_inner`/`cluster_delete_object_inner` already use.
    fn get_metadata(&self, user_id: &str, bucket: &str, object_id: &str) -> Result<Metadata, Error> {
        if let Some(record) = self.content_location_store.get(user_id, bucket, object_id) {
            return Ok(Metadata {
                chunks: Vec::new(),
                properties: Default::default(),
                etag: record.etag,
                size: record.original_len as u64,
                content_type: record.content_type,
                last_modified: record.last_modified,
                user_metadata: record.user_metadata,
                cache_control: record.cache_control,
                expires: record.expires,
                content_encoding: record.content_encoding,
                version_id: None,
                is_delete_marker: false,
                checksum_algorithm: record.checksum_algorithm,
                checksum_value: record.checksum_value,
                checksum_type: record.checksum_type,
            });
        }
        if let Some(record) = self.location_store.get(user_id, bucket, object_id) {
            return Ok(Metadata {
                chunks: Vec::new(),
                properties: Default::default(),
                etag: record.etag,
                size: record.original_len as u64,
                content_type: record.content_type,
                last_modified: record.last_modified,
                user_metadata: record.user_metadata,
                cache_control: record.cache_control,
                expires: record.expires,
                content_encoding: record.content_encoding,
                version_id: None,
                is_delete_marker: false,
                checksum_algorithm: record.checksum_algorithm,
                checksum_value: record.checksum_value,
                checksum_type: record.checksum_type,
            });
        }
        Err(Self::not_found(bucket, object_id))
    }

    fn delete_metadata(&self, _user_id: &str, _bucket: &str, _object_id: &str) -> Result<(), Error> {
        Err(not_supported("delete_metadata"))
    }

    fn list_objects(&self, user_id: &str, bucket: &str) -> Result<Vec<ObjectId>, Error> {
        let mut keys: Vec<String> = self.location_store.list(user_id, bucket, "").into_iter().map(|(k, _)| k).collect();
        keys.extend(self.content_location_store.list(user_id, bucket, "").into_iter().map(|(k, _)| k));
        keys.sort();
        keys.dedup();
        Ok(keys)
    }

    fn object_exists(&self, user_id: &str, bucket: &str, object_id: &str) -> Result<bool, Error> {
        Ok(self.content_location_store.get(user_id, bucket, object_id).is_some()
            || self.location_store.get(user_id, bucket, object_id).is_some())
    }

    fn update_metadata(&self, _user_id: &str, _bucket: &str, _object_id: &str, _metadata: &Metadata) -> Result<(), Error> {
        Err(not_supported("update_metadata"))
    }

    fn update_object_id(&self, _user_id: &str, _bucket: &str, _old_object_id: &str, _new_object_id: &str) -> Result<(), Error> {
        // A rename would need to re-place the object (shard_peers are a
        // deterministic hash of the key, see placement.rs) -- not a
        // metadata-only operation here the way a SQL UPDATE is.
        Err(not_supported("update_object_id"))
    }

    fn queue_deletion(&self, _user_id: &str, _bucket: &str, _key: &str, _offset_size_list: &[(u64, u64)]) -> Result<(), Error> {
        Err(not_supported("queue_deletion (no deletion-GC worker exists in cluster mode)"))
    }

    // No bucket registry in cluster mode -- any bucket name is implicitly
    // valid (see `bucket_exists`), so there is nothing truthful to list.
    fn list_buckets_with_stats(&self, _user_id: &str) -> Result<Vec<BucketStats>, Error> {
        Ok(Vec::new())
    }

    // Deliberate simplification, not an oversight: cluster mode has no
    // bucket registry today (bucket_config.rs is placement *config*, not
    // existence tracking) -- every bucket name already works implicitly
    // on PUT. create/delete are no-ops so callers that expect S3's
    // CreateBucket/DeleteBucket semantics don't get a hard error; a real
    // bucket registry is future work, not something to fake here.
    fn create_bucket(&self, _user_id: &str, _bucket: &str) -> Result<(), Error> {
        Ok(())
    }

    fn delete_bucket(&self, _user_id: &str, _bucket: &str) -> Result<(), Error> {
        Ok(())
    }

    fn bucket_exists(&self, _user_id: &str, _bucket: &str) -> Result<bool, Error> {
        Ok(true)
    }

    fn list_all_buckets_for_user(&self, _user_id: &str) -> Result<Vec<String>, Error> {
        Ok(Vec::new())
    }

    fn bucket_object_stats(&self, user_id: &str, bucket: &str) -> Result<(u64, u64), Error> {
        let plain = self.location_store.list(user_id, bucket, "");
        let packed = self.content_location_store.list(user_id, bucket, "");
        let count = (plain.len() + packed.len()) as u64;
        let bytes: u64 = plain.iter().chain(packed.iter()).map(|(_, size)| size).sum();
        Ok((count, bytes))
    }

    fn put_object_v2(&self, _user_id: &str, _bucket: &str, _key: &str, _metadata: &Metadata) -> Result<(Option<String>, Vec<(u64, u64)>), Error> {
        Err(not_supported("put_object_v2"))
    }

    fn get_pending_deletions(&self, _limit: i32) -> Result<Vec<DeletionEvent>, Error> {
        Err(not_supported("get_pending_deletions (no deletion-GC worker exists in cluster mode)"))
    }

    fn mark_deletion_processed(&self, _id: i64) -> Result<(), Error> {
        Err(not_supported("mark_deletion_processed (no deletion-GC worker exists in cluster mode)"))
    }

    fn cleanup_old_deletions(&self) -> Result<usize, Error> {
        Err(not_supported("cleanup_old_deletions (no deletion-GC worker exists in cluster mode)"))
    }

    fn set_bucket_cors(&self, _bucket: &str, _cors_xml: &str) -> Result<(), Error> {
        Err(not_supported("bucket CORS configuration"))
    }

    fn get_bucket_cors(&self, _bucket: &str) -> Result<Option<String>, Error> {
        Err(not_supported("bucket CORS configuration"))
    }

    fn delete_bucket_cors(&self, _bucket: &str) -> Result<(), Error> {
        Err(not_supported("bucket CORS configuration"))
    }

    fn set_bucket_location(&self, _user_id: &str, _bucket: &str, _location: &str) -> Result<(), Error> {
        Err(not_supported("bucket location configuration"))
    }

    fn get_bucket_location(&self, _user_id: &str, _bucket: &str) -> Result<String, Error> {
        Err(not_supported("bucket location configuration"))
    }

    fn get_versioning_state(&self, _bucket: &str) -> Result<String, Error> {
        Err(not_supported("versioning"))
    }

    fn set_versioning_state(&self, _user_id: &str, _bucket: &str, _state: &str) -> Result<(), Error> {
        Err(not_supported("versioning"))
    }

    fn delete_object_v2(&self, _user_id: &str, _bucket: &str, _key: &str) -> Result<VersioningDeleteResult, Error> {
        Err(not_supported("versioning"))
    }

    fn delete_specific_version(&self, _user_id: &str, _bucket: &str, _key: &str, _version_id: &str) -> Result<DeleteSpecificResult, Error> {
        Err(not_supported("versioning"))
    }

    fn get_object_version(&self, _user_id: &str, _bucket: &str, _key: &str, _version_id: &str) -> Result<Metadata, Error> {
        Err(not_supported("versioning"))
    }

    fn get_latest_last_modified(&self, _user_id: &str, _bucket: &str, _key: &str) -> Result<Option<String>, Error> {
        Err(not_supported("versioning"))
    }

    fn list_object_versions_full(
        &self, _user_id: &str, _bucket: &str,
        _prefix: &str, _key_marker: &str, _version_id_marker: &str, _max_keys: usize,
    ) -> Result<(Vec<VersionRow>, bool, String, String), Error> {
        Err(not_supported("versioning"))
    }

    fn set_object_tags(&self, _user_id: &str, _bucket: &str, _key: &str, _tags: &[(String, String)]) -> Result<(), Error> {
        Err(not_supported("object tagging"))
    }

    fn get_object_tags(&self, _user_id: &str, _bucket: &str, _key: &str) -> Result<Vec<(String, String)>, Error> {
        Err(not_supported("object tagging"))
    }

    fn delete_object_tags(&self, _user_id: &str, _bucket: &str, _key: &str) -> Result<(), Error> {
        Err(not_supported("object tagging"))
    }

    fn get_object_tag_count(&self, _user_id: &str, _bucket: &str, _key: &str) -> Result<i64, Error> {
        Err(not_supported("object tagging"))
    }

    fn set_bucket_tags(&self, _bucket: &str, _tags: &[(String, String)]) -> Result<(), Error> {
        Err(not_supported("bucket tagging"))
    }

    fn get_bucket_tags(&self, _bucket: &str) -> Result<Vec<(String, String)>, Error> {
        Err(not_supported("bucket tagging"))
    }

    fn delete_bucket_tags(&self, _bucket: &str) -> Result<(), Error> {
        Err(not_supported("bucket tagging"))
    }

    fn set_multipart_tagging(&self, _upload_id: &str, _tagging: &str) -> Result<(), Error> {
        Err(not_supported("multipart upload"))
    }

    fn get_multipart_tagging(&self, _upload_id: &str) -> Result<String, Error> {
        Err(not_supported("multipart upload"))
    }

    fn create_multipart_upload(
        &self, _upload_id: &str, _user_id: &str, _bucket: &str, _key: &str,
        _content_type: Option<&str>, _metadata_json: &str, _initiated_at: &str,
        _checksum_algorithm: &str, _checksum_type: &str,
        _object_lock_mode: &str, _object_lock_retain_until: &str, _object_lock_legal_hold: &str,
    ) -> Result<(), Error> {
        Err(not_supported("multipart upload"))
    }

    fn get_multipart_upload(&self, _upload_id: &str) -> Result<Option<MultipartUploadRow>, Error> {
        Err(not_supported("multipart upload"))
    }

    fn mark_multipart_completed(&self, _upload_id: &str, _final_etag: &str) -> Result<(), Error> {
        Err(not_supported("multipart upload"))
    }

    fn delete_multipart_upload(&self, _upload_id: &str) -> Result<(), Error> {
        Err(not_supported("multipart upload"))
    }

    fn delete_completed_uploads_for_key(&self, _bucket: &str, _key: &str) -> Result<(), Error> {
        Err(not_supported("multipart upload"))
    }

    fn list_bucket_multipart_uploads(&self, _bucket: &str) -> Result<Vec<MultipartUploadRow>, Error> {
        Err(not_supported("multipart upload"))
    }

    fn upsert_multipart_part(
        &self, _upload_id: &str, _part_number: i32, _etag: &str, _size: u64, _extents_blob: &[u8],
        _checksum_value: &str,
    ) -> Result<(), Error> {
        Err(not_supported("multipart upload"))
    }

    fn list_multipart_parts(&self, _upload_id: &str) -> Result<Vec<MultipartPartRow>, Error> {
        Err(not_supported("multipart upload"))
    }

    fn delete_parts_for_upload(&self, _upload_id: &str) -> Result<(), Error> {
        Err(not_supported("multipart upload"))
    }

    fn get_parts_manifest(&self, _user_id: &str, _bucket: &str, _key: &str) -> Result<Option<String>, Error> {
        Err(not_supported("multipart upload"))
    }

    fn set_parts_manifest(&self, _user_id: &str, _bucket: &str, _key: &str, _manifest: &str) -> Result<(), Error> {
        Err(not_supported("multipart upload"))
    }

    // Cluster object lock already has its own real mechanism
    // (LocationRecord.retention_mode/retain_until/legal_hold, enforced by
    // cluster_delete_object_inner directly) -- deliberately not bridged
    // to this trait's object-lock methods, which stay unsupported here.
    fn get_bucket_object_lock_enabled(&self, _bucket: &str) -> Result<bool, Error> {
        Err(not_supported("object-lock bucket configuration (cluster enforces retention directly on LocationRecord, not via this trait)"))
    }

    fn set_bucket_object_lock_enabled(&self, _bucket: &str, _enabled: bool) -> Result<(), Error> {
        Err(not_supported("object-lock bucket configuration (cluster enforces retention directly on LocationRecord, not via this trait)"))
    }

    fn create_bucket_with_lock(&self, _user_id: &str, _bucket: &str, _lock_enabled: bool) -> Result<(), Error> {
        Err(not_supported("object-lock bucket configuration (cluster enforces retention directly on LocationRecord, not via this trait)"))
    }

    fn get_object_lock_config(&self, _bucket: &str) -> Result<Option<(String, Option<i64>, Option<i64>)>, Error> {
        Err(not_supported("object-lock bucket configuration (cluster enforces retention directly on LocationRecord, not via this trait)"))
    }

    fn put_object_lock_config(&self, _bucket: &str, _mode: &str, _days: Option<i64>, _years: Option<i64>) -> Result<(), Error> {
        Err(not_supported("object-lock bucket configuration (cluster enforces retention directly on LocationRecord, not via this trait)"))
    }

    fn get_object_lock(&self, _bucket: &str, _key: &str, _version_id: &str) -> Result<Option<ObjectLockRow>, Error> {
        Err(not_supported("object-lock (cluster enforces retention directly on LocationRecord, not via this trait)"))
    }

    fn put_object_lock(
        &self, _bucket: &str, _key: &str, _version_id: &str,
        _mode: Option<&str>, _retain_until_date: Option<&str>, _legal_hold: Option<&str>,
    ) -> Result<(), Error> {
        Err(not_supported("object-lock (cluster enforces retention directly on LocationRecord, not via this trait)"))
    }

    fn set_object_legal_hold(&self, _bucket: &str, _key: &str, _version_id: &str, _status: &str) -> Result<(), Error> {
        Err(not_supported("object-lock (cluster enforces retention directly on LocationRecord, not via this trait)"))
    }

    fn check_object_lock_protection(
        &self, _bucket: &str, _key: &str, _version_id: &str, _bypass_governance: bool,
    ) -> Result<(bool, bool), Error> {
        Err(not_supported("object-lock (cluster enforces retention directly on LocationRecord, not via this trait)"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::content_location_store::BitcaskContentLocationStore;
    use crate::cluster::location_store::{BitcaskLocationStore, LocationRecord, ObjectS3Metadata};
    use std::collections::HashMap;

    fn sample_record(user_id: &str, bucket: &str, key: &str, size: usize) -> LocationRecord {
        let meta = ObjectS3Metadata {
            etag: Some("\"abc123\"".to_string()),
            content_type: Some("text/plain".to_string()),
            last_modified: Some("Mon, 01 Jan 2026 00:00:00 GMT".to_string()),
            user_metadata: HashMap::new(),
            cache_control: None,
            expires: None,
            content_encoding: None,
            checksum_algorithm: None,
            checksum_value: None,
            checksum_type: None,
        };
        LocationRecord {
            user_id: user_id.to_string(),
            bucket: bucket.to_string(),
            key: key.to_string(),
            shard_peers: vec!["http://node0:9710".into(), "http://node1:9710".into()],
            k: 1,
            m: 1,
            original_len: size,
            version: "v1".to_string(),
            retention_mode: None,
            retain_until: None,
            legal_hold: false,
            etag: meta.etag,
            content_type: meta.content_type,
            last_modified: meta.last_modified,
            user_metadata: meta.user_metadata,
            cache_control: meta.cache_control,
            expires: meta.expires,
            content_encoding: meta.content_encoding,
            checksum_algorithm: meta.checksum_algorithm,
            checksum_value: meta.checksum_value,
            checksum_type: meta.checksum_type,
        }
    }

    fn store() -> ClusterMetadataStore {
        let dir = tempfile::tempdir().unwrap();
        let loc = Arc::new(BitcaskLocationStore::open(dir.path().join("loc.log")).unwrap());
        let cloc = Arc::new(BitcaskContentLocationStore::open(dir.path().join("cloc.log")).unwrap());
        ClusterMetadataStore::new(loc, cloc)
    }

    #[test]
    fn get_metadata_translates_a_plain_record() {
        let s = store();
        s.location_store.put(sample_record("u1", "b1", "k1", 42)).unwrap();

        let meta = s.get_metadata("u1", "b1", "k1").unwrap();
        assert_eq!(meta.etag, Some("\"abc123\"".to_string()));
        assert_eq!(meta.content_type, Some("text/plain".to_string()));
        assert_eq!(meta.size, 42);
    }

    #[test]
    fn get_metadata_not_found_matches_sqlite_stores_message_shape() {
        let s = store();
        let err = s.get_metadata("u1", "b1", "missing").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("No data found for key: missing in bucket: b1"), "{msg}");
    }

    #[test]
    fn object_exists_checks_both_stores() {
        let s = store();
        assert!(!s.object_exists("u1", "b1", "k1").unwrap());
        s.location_store.put(sample_record("u1", "b1", "k1", 10)).unwrap();
        assert!(s.object_exists("u1", "b1", "k1").unwrap());
    }

    #[test]
    fn list_objects_merges_and_dedupes_both_stores() {
        let s = store();
        s.location_store.put(sample_record("u1", "b1", "k1", 10)).unwrap();
        s.location_store.put(sample_record("u1", "b1", "k2", 10)).unwrap();

        let keys = s.list_objects("u1", "b1").unwrap();
        assert_eq!(keys, vec!["k1".to_string(), "k2".to_string()]);
    }

    #[test]
    fn list_objects_is_isolated_per_user() {
        let s = store();
        s.location_store.put(sample_record("alice", "b1", "k1", 10)).unwrap();
        s.location_store.put(sample_record("bob", "b1", "k1", 10)).unwrap();

        assert_eq!(s.list_objects("alice", "b1").unwrap(), vec!["k1".to_string()]);
        assert_eq!(s.list_objects("bob", "b1").unwrap(), vec!["k1".to_string()]);
    }

    #[test]
    fn bucket_object_stats_sums_sizes_across_both_stores() {
        let s = store();
        s.location_store.put(sample_record("u1", "b1", "k1", 10)).unwrap();
        s.location_store.put(sample_record("u1", "b1", "k2", 20)).unwrap();

        let (count, bytes) = s.bucket_object_stats("u1", "b1").unwrap();
        assert_eq!(count, 2);
        assert_eq!(bytes, 30);
    }

    #[test]
    fn bucket_exists_is_always_true_no_registry_in_cluster_mode() {
        let s = store();
        assert!(s.bucket_exists("u1", "any-bucket-name").unwrap());
    }

    #[test]
    fn write_shaped_methods_return_explicit_errors_not_panics() {
        let s = store();
        assert!(s.put_metadata("u1", "b1", "k1", &Metadata::from_offset_size_list(vec![])).is_err());
        assert!(s.put_object_v2("u1", "b1", "k1", &Metadata::from_offset_size_list(vec![])).is_err());
        assert!(s.update_metadata("u1", "b1", "k1", &Metadata::from_offset_size_list(vec![])).is_err());
        assert!(s.update_object_id("u1", "b1", "k1", "k2").is_err());
        assert!(s.delete_metadata("u1", "b1", "k1").is_err());
    }

    #[test]
    fn unsupported_feature_methods_return_explicit_errors_not_panics() {
        let s = store();
        assert!(s.get_versioning_state("b1").is_err());
        assert!(s.get_object_tags("u1", "b1", "k1").is_err());
        assert!(s.get_multipart_upload("upload1").is_err());
        assert!(s.get_object_lock("b1", "k1", "").is_err());
        assert!(s.get_bucket_cors("b1").is_err());
        assert!(s.get_pending_deletions(10).is_err());
    }
}
