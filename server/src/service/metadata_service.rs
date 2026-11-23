//! Metadata service layer bridging handlers with the MetadataStorage trait

use crate::metadata::{MetadataStorage, Metadata, BucketStats, config::MetadataConfig};
use std::sync::Arc;
use actix_web::Error;

static METADATA_STORE: std::sync::OnceLock<Arc<dyn MetadataStorage>> = std::sync::OnceLock::new();

/// Sets the store every `MetadataService` uses. Called once at startup: a
/// cluster node sets the replicated store here, so the same handlers serve
/// both deployments. Returns false if already set.
pub fn set_metadata_store(store: Arc<dyn MetadataStorage>) -> bool {
    METADATA_STORE.set(store).is_ok()
}

/// The process-wide metadata store. Without `set_metadata_store`, the one
/// `METADATA_BACKEND` names (SQLite by default).
pub fn metadata_store() -> Arc<dyn MetadataStorage> {
    Arc::clone(METADATA_STORE.get_or_init(|| MetadataConfig::from_env().create_store()))
}

#[derive(Clone)]
pub struct MetadataService {
    user: String,
    store: Arc<dyn MetadataStorage>,
}

impl MetadataService {
    /// Single-node's existing entry point, unchanged behavior: backed by
    /// the process-wide `METADATA_STORE` singleton (`SQLiteMetadataStore`
    /// by default, `MockMetadataStore` under `METADATA_BACKEND=mock`).
    pub fn new(user: &str) -> Result<Self, Error> {
        Ok(Self { user: user.to_string(), store: metadata_store() })
    }

    /// Cluster's entry point: the same facade, the same S3-handler-facing
    /// API, backed by a caller-supplied store instead of the single-node
    /// singleton -- this is what lets the existing S3 protocol handlers
    /// serve cluster traffic without any of them changing, once they're
    /// parameterized to build a `MetadataService` this way instead of via
    /// `new`. See `cluster::metadata_store::ClusterMetadataStore`.
    pub fn with_store(user: &str, store: Arc<dyn MetadataStorage>) -> Result<Self, Error> {
        Ok(Self { user: user.to_string(), store })
    }

    // --- Object existence / key checks ---

    pub fn check_key(&self, bucket: &str, key: &str) -> Result<bool, Error> {
        self.store.object_exists(&self.user, bucket, key)
    }

    pub fn check_key_nonexistance(&self, bucket: &str, key: &str) -> Result<(), Error> {
        if !self.check_key(bucket, key)? {
            return Err(actix_web::error::ErrorNotFound(format!(
                "No data found for key: {} in bucket: {}, The key does not exist",
                key, bucket
            )));
        }
        Ok(())
    }

    // --- Full-metadata S3 path (includes etag, size, content_type, etc.) ---

    /// Write a fully-populated Metadata object (S3 PUT path).
    /// Returns (version_id, old_extents_to_gc):
    ///   - version_id: Some(vid) when versioning enabled/suspended, None when disabled.
    ///   - old_extents_to_gc: storage extents of the row that was replaced (queue for GC).
    pub fn put_object_full(
        &self, bucket: &str, key: &str, metadata: Metadata,
    ) -> Result<(Option<String>, Vec<(u64, u64)>), Error> {
        self.store.put_object_v2(&self.user, bucket, key, &metadata)
    }

    /// `put_object_full` without blocking the caller's thread when the store
    /// has an async path (the replicated cluster store). The write runs on
    /// the bridge runtime and the caller awaits it.
    pub async fn put_object_full_async(
        &self, bucket: &str, key: &str, metadata: Metadata,
    ) -> Result<(Option<String>, Vec<(u64, u64)>), Error> {
        match Arc::clone(&self.store).put_object_v2_async(self.user.clone(), bucket.to_string(), key.to_string(), metadata) {
            Ok(fut) => crate::cluster::bridge::spawn(fut)
                .await
                .map_err(|e| actix_web::error::ErrorInternalServerError(format!("metadata write task failed: {e}")))?
                .map_err(actix_web::error::ErrorInternalServerError),
            Err(metadata) => self.store.put_object_v2(&self.user, bucket, key, &metadata),
        }
    }

    /// The S3 DeleteObject metadata write, awaited without blocking the
    /// caller's thread, when the store has an async path. `None` otherwise.
    pub fn delete_object_async(&self, bucket: &str, key: &str) -> Option<impl std::future::Future<Output = Result<crate::metadata::VersioningDeleteResult, Error>>> {
        let fut = Arc::clone(&self.store).delete_object_async(self.user.clone(), bucket.to_string(), key.to_string())?;
        Some(async move {
            crate::cluster::bridge::spawn(fut)
                .await
                .map_err(|e| actix_web::error::ErrorInternalServerError(format!("metadata write task failed: {e}")))?
                .map_err(actix_web::error::ErrorInternalServerError)
        })
    }

    /// `upsert_multipart_part`, awaited without blocking the caller's thread
    /// when the store has an async path.
    pub async fn upsert_multipart_part_awaited(
        &self, upload_id: &str, part_number: i32, etag: &str, size: u64, extents_blob: &[u8], checksum_value: &str,
    ) -> Result<(), Error> {
        let fut = Arc::clone(&self.store).upsert_multipart_part_async(
            upload_id.to_string(), part_number, etag.to_string(), size, extents_blob.to_vec(), checksum_value.to_string(),
        );
        match fut {
            Some(fut) => crate::cluster::bridge::spawn(fut)
                .await
                .map_err(|e| actix_web::error::ErrorInternalServerError(format!("metadata write task failed: {e}")))?
                .map_err(|(status, msg)| {
                    let status = actix_web::http::StatusCode::from_u16(status).unwrap_or(actix_web::http::StatusCode::INTERNAL_SERVER_ERROR);
                    Error::from(actix_web::error::InternalError::new(msg, status))
                }),
            None => self.store.upsert_multipart_part(upload_id, part_number, etag, size, extents_blob, checksum_value),
        }
    }

    /// Runs a metadata call on the blocking pool, so an HTTP worker never
    /// waits on it. For calls without an async path: on a cluster each
    /// write waits on a replication round, and with one worker per core a
    /// few writes waiting on a slow peer would stall every request on the
    /// node. The error keeps its HTTP status.
    pub async fn call<T: Send + 'static>(&self, f: impl FnOnce(&MetadataService) -> Result<T, Error> + Send + 'static) -> Result<T, Error> {
        let db = self.clone();
        web_block_keep_status(move || f(&db)).await
    }

    /// `delete_specific_version`, awaited without blocking the caller's
    /// thread.
    pub async fn delete_specific_version_awaited(&self, bucket: &str, key: &str, version_id: &str) -> Result<crate::metadata::DeleteSpecificResult, Error> {
        match Arc::clone(&self.store).delete_specific_version_async(self.user.clone(), bucket.to_string(), key.to_string(), version_id.to_string()) {
            Some(fut) => crate::cluster::bridge::spawn(fut)
                .await
                .map_err(|e| actix_web::error::ErrorInternalServerError(format!("metadata write task failed: {e}")))?
                .map_err(actix_web::error::ErrorInternalServerError),
            None => {
                let (b, k, v) = (bucket.to_string(), key.to_string(), version_id.to_string());
                self.call(move |db| db.delete_specific_version(&b, &k, &v)).await
            }
        }
    }

    /// Read a fully-populated Metadata object (S3 GET / HEAD path).
    pub fn get_object_full(&self, bucket: &str, key: &str) -> Result<Metadata, Error> {
        self.store.get_metadata(&self.user, bucket, key)
    }

    // --- Legacy bytes-based path (old native API and internal use) ---

    pub fn write_metadata(&self, bucket: &str, key: &str, offset_size_bytes: &[u8]) -> Result<(), Error> {
        use crate::util::serializer::deserialize_offset_size;
        let offset_size_list = deserialize_offset_size(offset_size_bytes)?;
        let metadata = Metadata::from_offset_size_list(offset_size_list);
        self.store.put_metadata(&self.user, bucket, key, &metadata)
    }

    pub fn read_metadata(&self, bucket: &str, key: &str) -> Result<Vec<u8>, Error> {
        use crate::util::serializer::serialize_offset_size;
        let metadata = self.store.get_metadata(&self.user, bucket, key)?;
        let offset_size_list = metadata.to_offset_size_list();
        serialize_offset_size(&offset_size_list)
    }

    pub fn delete_metadata(&self, bucket: &str, key: &str) -> Result<(), Error> {
        self.store.delete_metadata(&self.user, bucket, key)
    }

    pub fn rename_key(&self, bucket: &str, old_key: &str, new_key: &str) -> Result<(), Error> {
        self.store.update_object_id(&self.user, bucket, old_key, new_key)
    }

    pub fn update_metadata(&self, bucket: &str, key: &str, offset_size_bytes: &[u8]) -> Result<(), Error> {
        use crate::util::serializer::deserialize_offset_size;
        let offset_size_list = deserialize_offset_size(offset_size_bytes)?;
        let metadata = Metadata::from_offset_size_list(offset_size_list);
        self.store.update_metadata(&self.user, bucket, key, &metadata)
    }

    pub fn append_metadata(&self, bucket: &str, key: &str, offset_size_bytes: &[u8]) -> Result<(), Error> {
        self.update_metadata(bucket, key, offset_size_bytes)
    }

    pub fn list_objects(&self, bucket: &str) -> Result<Vec<String>, Error> {
        self.store.list_objects(&self.user, bucket)
    }

    pub fn list_objects_with_prefix(&self, bucket: &str, prefix: &str) -> Result<Vec<String>, Error> {
        self.store.list_objects_with_prefix(&self.user, bucket, prefix)
    }

    // --- Bucket management ---

    pub fn create_bucket(&self, bucket: &str) -> Result<(), Error> {
        self.store.create_bucket(&self.user, bucket)
    }

    pub fn delete_bucket(&self, bucket: &str) -> Result<(), Error> {
        self.store.delete_bucket(&self.user, bucket)
    }

    pub fn bucket_exists(&self, bucket: &str) -> Result<bool, Error> {
        self.store.bucket_exists(&self.user, bucket)
    }

    pub fn list_all_buckets(&self) -> Result<Vec<String>, Error> {
        self.store.list_all_buckets_for_user(&self.user)
    }

    // --- Stats ---

    pub fn list_buckets_with_stats(&self) -> Result<Vec<BucketStats>, Error> {
        self.store.list_buckets_with_stats(&self.user)
    }

    pub fn bucket_object_stats(&self, bucket: &str) -> Result<(u64, u64), Error> {
        self.store.bucket_object_stats(&self.user, bucket)
    }

    // --- Deletion WAL ---

    pub fn queue_deletion(&self, bucket: &str, key: &str, offset_size_list: &[(u64, u64)]) -> Result<(), Error> {
        self.store.queue_deletion(&self.user, bucket, key, offset_size_list)
    }

    pub fn get_pending_deletions(&self, limit: i32) -> Result<Vec<crate::metadata::sqlite_store::DeletionEvent>, Error> {
        self.store.get_pending_deletions(limit)
    }

    pub fn mark_deletion_processed(&self, id: i64) -> Result<(), Error> {
        self.store.mark_deletion_processed(id)
    }

    pub fn cleanup_old_deletions(&self) -> Result<usize, Error> {
        self.store.cleanup_old_deletions()
    }

    // --- CORS ---

    pub fn set_bucket_cors(&self, bucket: &str, cors_xml: &str) -> Result<(), Error> {
        self.store.set_bucket_cors(bucket, cors_xml)
    }

    pub fn get_bucket_cors(&self, bucket: &str) -> Result<Option<String>, Error> {
        self.store.get_bucket_cors(bucket)
    }

    pub fn delete_bucket_cors(&self, bucket: &str) -> Result<(), Error> {
        self.store.delete_bucket_cors(bucket)
    }

    // --- Bucket location ---

    pub fn set_bucket_location(&self, bucket: &str, location: &str) -> Result<(), Error> {
        self.store.set_bucket_location(&self.user, bucket, location)
    }

    pub fn get_bucket_location(&self, bucket: &str) -> Result<String, Error> {
        self.store.get_bucket_location(&self.user, bucket)
    }

    // --- Versioning ---

    pub fn get_versioning_state(&self, bucket: &str) -> Result<String, Error> {
        self.store.get_versioning_state(bucket)
    }

    pub fn set_versioning_state(&self, bucket: &str, state: &str) -> Result<(), Error> {
        self.store.set_versioning_state(&self.user, bucket, state)
    }

    pub fn delete_object_v2(&self, bucket: &str, key: &str)
        -> Result<crate::metadata::sqlite_store::VersioningDeleteResult, Error>
    {
        self.store.delete_object_v2(&self.user, bucket, key)
    }

    pub fn delete_specific_version(&self, bucket: &str, key: &str, version_id: &str)
        -> Result<crate::metadata::sqlite_store::DeleteSpecificResult, Error>
    {
        self.store.delete_specific_version(&self.user, bucket, key, version_id)
    }

    pub fn get_object_version(&self, bucket: &str, key: &str, version_id: &str)
        -> Result<crate::metadata::Metadata, Error>
    {
        self.store.get_object_version(&self.user, bucket, key, version_id)
    }

    /// Returns the last_modified of the is_latest=1 row, including delete markers.
    pub fn get_latest_last_modified(&self, bucket: &str, key: &str) -> Result<Option<String>, Error> {
        self.store.get_latest_last_modified(&self.user, bucket, key)
    }

    /// Returns `(version_id, is_delete_marker)` of the is_latest=1 row, including
    /// delete markers.
    pub fn get_latest_version_marker_info(&self, bucket: &str, key: &str) -> Result<Option<(String, bool)>, Error> {
        self.store.get_latest_version_marker_info(&self.user, bucket, key)
    }

    pub fn list_object_versions_full(
        &self, bucket: &str, prefix: &str, key_marker: &str,
        version_id_marker: &str, max_keys: usize,
    ) -> Result<(Vec<crate::metadata::sqlite_store::VersionRow>, bool, String, String), Error> {
        self.store.list_object_versions_full(
            &self.user, bucket, prefix, key_marker, version_id_marker, max_keys,
        )
    }

    // --- Tagging ---

    pub fn set_object_tags(&self, bucket: &str, key: &str, tags: &[(String, String)]) -> Result<(), Error> {
        self.store.set_object_tags(&self.user, bucket, key, tags)
    }

    pub fn get_object_tags(&self, bucket: &str, key: &str) -> Result<Vec<(String, String)>, Error> {
        self.store.get_object_tags(&self.user, bucket, key)
    }

    pub fn delete_object_tags(&self, bucket: &str, key: &str) -> Result<(), Error> {
        self.store.delete_object_tags(&self.user, bucket, key)
    }

    pub fn get_object_tag_count(&self, bucket: &str, key: &str) -> Result<i64, Error> {
        self.store.get_object_tag_count(&self.user, bucket, key)
    }

    pub fn set_bucket_tags(&self, bucket: &str, tags: &[(String, String)]) -> Result<(), Error> {
        self.store.set_bucket_tags(bucket, tags)
    }

    pub fn get_bucket_tags(&self, bucket: &str) -> Result<Vec<(String, String)>, Error> {
        self.store.get_bucket_tags(bucket)
    }

    pub fn delete_bucket_tags(&self, bucket: &str) -> Result<(), Error> {
        self.store.delete_bucket_tags(bucket)
    }

    pub fn set_multipart_tagging(&self, upload_id: &str, tagging: &str) -> Result<(), Error> {
        self.store.set_multipart_tagging(upload_id, tagging)
    }

    pub fn get_multipart_tagging(&self, upload_id: &str) -> Result<String, Error> {
        self.store.get_multipart_tagging(upload_id)
    }

    // --- ACL ---

    pub fn set_bucket_acl(&self, bucket: &str, owner_id: &str, grants_json: &str) -> Result<(), Error> {
        self.store.set_bucket_acl(bucket, owner_id, grants_json)
    }

    pub fn get_bucket_acl(&self, bucket: &str) -> Result<Option<(String, String)>, Error> {
        self.store.get_bucket_acl(bucket)
    }

    pub fn set_object_acl(&self, bucket: &str, key: &str, version_id: &str, owner_id: &str, grants_json: &str) -> Result<(), Error> {
        self.store.set_object_acl(bucket, key, version_id, owner_id, grants_json)
    }

    pub fn get_object_acl(&self, bucket: &str, key: &str, version_id: &str) -> Result<Option<(String, String)>, Error> {
        self.store.get_object_acl(bucket, key, version_id)
    }

    pub fn set_public_access_block(&self, bucket: &str, block_public_acls: bool, ignore_public_acls: bool, block_public_policy: bool, restrict_public_buckets: bool) -> Result<(), Error> {
        self.store.set_public_access_block(bucket, block_public_acls, ignore_public_acls, block_public_policy, restrict_public_buckets)
    }

    pub fn get_public_access_block(&self, bucket: &str) -> Result<Option<(bool, bool, bool, bool)>, Error> {
        self.store.get_public_access_block(bucket)
    }

    pub fn delete_public_access_block(&self, bucket: &str) -> Result<(), Error> {
        self.store.delete_public_access_block(bucket)
    }

    // --- Multipart upload management ---

    pub fn create_multipart_upload(
        &self, upload_id: &str, bucket: &str, key: &str,
        content_type: Option<&str>, metadata_json: &str, initiated_at: &str,
        checksum_algorithm: &str, checksum_type: &str,
        object_lock_mode: &str, object_lock_retain_until: &str, object_lock_legal_hold: &str,
    ) -> Result<(), Error> {
        self.store.create_multipart_upload(
            upload_id, &self.user, bucket, key, content_type, metadata_json, initiated_at,
            checksum_algorithm, checksum_type,
            object_lock_mode, object_lock_retain_until, object_lock_legal_hold,
        )
    }

    pub fn get_multipart_upload(&self, upload_id: &str)
        -> Result<Option<crate::metadata::sqlite_store::MultipartUploadRow>, Error>
    {
        self.store.get_multipart_upload(upload_id)
    }

    pub fn mark_multipart_completed(&self, upload_id: &str, final_etag: &str) -> Result<(), Error> {
        self.store.mark_multipart_completed(upload_id, final_etag)
    }

    pub fn delete_multipart_upload(&self, upload_id: &str) -> Result<(), Error> {
        self.store.delete_multipart_upload(upload_id)
    }

    pub fn delete_completed_uploads_for_key(&self, bucket: &str, key: &str) -> Result<(), Error> {
        self.store.delete_completed_uploads_for_key(bucket, key)
    }

    pub fn list_multipart_uploads_for_bucket(&self, bucket: &str)
        -> Result<Vec<crate::metadata::sqlite_store::MultipartUploadRow>, Error>
    {
        self.store.list_bucket_multipart_uploads(bucket)
    }

    pub fn upsert_multipart_part(
        &self, upload_id: &str, part_number: i32, etag: &str, size: u64, extents_blob: &[u8],
        checksum_value: &str,
    ) -> Result<(), Error> {
        self.store.upsert_multipart_part(upload_id, part_number, etag, size, extents_blob, checksum_value)
    }

    pub fn list_multipart_parts(&self, upload_id: &str)
        -> Result<Vec<crate::metadata::sqlite_store::MultipartPartRow>, Error>
    {
        self.store.list_multipart_parts(upload_id)
    }

    pub fn delete_parts_for_upload(&self, upload_id: &str) -> Result<(), Error> {
        self.store.delete_parts_for_upload(upload_id)
    }

    pub fn get_parts_manifest(&self, bucket: &str, key: &str) -> Result<Option<String>, Error> {
        self.store.get_parts_manifest(&self.user, bucket, key)
    }

    pub fn set_parts_manifest(&self, bucket: &str, key: &str, manifest: &str) -> Result<(), Error> {
        self.store.set_parts_manifest(&self.user, bucket, key, manifest)
    }

    // --- Object Lock ---

    pub fn get_bucket_object_lock_enabled(&self, bucket: &str) -> Result<bool, Error> {
        self.store.get_bucket_object_lock_enabled(bucket)
    }

    pub fn set_bucket_object_lock_enabled(&self, bucket: &str, enabled: bool) -> Result<(), Error> {
        self.store.set_bucket_object_lock_enabled(bucket, enabled)
    }

    pub fn create_bucket_with_lock(&self, bucket: &str, lock_enabled: bool) -> Result<(), Error> {
        self.store.create_bucket_with_lock(&self.user, bucket, lock_enabled)
    }

    pub fn get_object_lock_config(&self, bucket: &str) -> Result<Option<(String, Option<i64>, Option<i64>)>, Error> {
        self.store.get_object_lock_config(bucket)
    }

    pub fn put_object_lock_config(&self, bucket: &str, mode: &str, days: Option<i64>, years: Option<i64>) -> Result<(), Error> {
        self.store.put_object_lock_config(bucket, mode, days, years)
    }

    pub fn get_object_lock(&self, bucket: &str, key: &str, version_id: &str)
        -> Result<Option<crate::metadata::sqlite_store::ObjectLockRow>, Error>
    {
        self.store.get_object_lock(bucket, key, version_id)
    }

    pub fn put_object_lock(
        &self, bucket: &str, key: &str, version_id: &str,
        mode: Option<&str>, retain_until_date: Option<&str>, legal_hold: Option<&str>,
    ) -> Result<(), Error> {
        self.store.put_object_lock(bucket, key, version_id, mode, retain_until_date, legal_hold)
    }

    pub fn set_object_legal_hold(&self, bucket: &str, key: &str, version_id: &str, status: &str) -> Result<(), Error> {
        self.store.set_object_legal_hold(bucket, key, version_id, status)
    }

    pub fn check_object_lock_protection(
        &self, bucket: &str, key: &str, version_id: &str, bypass_governance: bool,
    ) -> Result<(bool, bool), Error> {
        self.store.check_object_lock_protection(bucket, key, version_id, bypass_governance)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::serializer::serialize_offset_size;
    use std::env;

    #[test]
    fn test_metadata_service_basic_operations() {
        env::set_var("METADATA_BACKEND", "mock");

        let service = MetadataService::new("test_user_service").unwrap();
        service.create_bucket("default").unwrap();
        let key = "test_key_service";

        assert!(!service.check_key("default", key).unwrap());
        assert!(service.check_key_nonexistance("default", key).is_err());

        let offset_size_list = vec![(100u64, 200u64), (300, 400)];
        let offset_size_bytes = serialize_offset_size(&offset_size_list).unwrap();

        service.write_metadata("default", key, &offset_size_bytes).unwrap();
        assert!(service.check_key("default", key).unwrap());

        let retrieved = service.read_metadata("default", key).unwrap();
        assert_eq!(retrieved, offset_size_bytes);

        service.delete_metadata("default", key).unwrap();
        assert!(!service.check_key("default", key).unwrap());

        env::remove_var("METADATA_BACKEND");
    }
}

/// `web::block` for a call returning actix's `Error`, which can't cross
/// threads: its status and message do, and it's rebuilt with both.
pub async fn web_block_keep_status<T: Send + 'static>(f: impl FnOnce() -> Result<T, Error> + Send + 'static) -> Result<T, Error> {
    actix_web::web::block(move || f().map_err(|e| (e.as_response_error().status_code(), e.to_string())))
        .await
        .map_err(|e| actix_web::error::ErrorInternalServerError(format!("blocking task failed: {e}")))?
        .map_err(|(status, msg)| Error::from(actix_web::error::InternalError::new(msg, status)))
}

// Awaited forms of the metadata writes that have no async path of their
// own: each runs on the blocking pool through `call`, so an HTTP worker
// never waits on a replication round. Same arguments as the plain form.
impl MetadataService {
    pub async fn delete_bucket_awaited(&self, bucket: &str) -> Result<(), Error> {
        let bucket = bucket.to_owned();
        self.call(move |db| db.delete_bucket(&bucket)).await
    }
    pub async fn set_bucket_cors_awaited(&self, bucket: &str, cors_xml: &str) -> Result<(), Error> {
        let bucket = bucket.to_owned(); let cors_xml = cors_xml.to_owned();
        self.call(move |db| db.set_bucket_cors(&bucket, &cors_xml)).await
    }
    pub async fn delete_bucket_cors_awaited(&self, bucket: &str) -> Result<(), Error> {
        let bucket = bucket.to_owned();
        self.call(move |db| db.delete_bucket_cors(&bucket)).await
    }
    pub async fn set_bucket_location_awaited(&self, bucket: &str, location: &str) -> Result<(), Error> {
        let bucket = bucket.to_owned(); let location = location.to_owned();
        self.call(move |db| db.set_bucket_location(&bucket, &location)).await
    }
    pub async fn set_versioning_state_awaited(&self, bucket: &str, state: &str) -> Result<(), Error> {
        let bucket = bucket.to_owned(); let state = state.to_owned();
        self.call(move |db| db.set_versioning_state(&bucket, &state)).await
    }
    pub async fn set_object_tags_awaited(&self, bucket: &str, key: &str, tags: &[(String, String)]) -> Result<(), Error> {
        let bucket = bucket.to_owned(); let key = key.to_owned(); let tags = tags.to_vec();
        self.call(move |db| db.set_object_tags(&bucket, &key, &tags)).await
    }
    pub async fn delete_object_tags_awaited(&self, bucket: &str, key: &str) -> Result<(), Error> {
        let bucket = bucket.to_owned(); let key = key.to_owned();
        self.call(move |db| db.delete_object_tags(&bucket, &key)).await
    }
    pub async fn set_bucket_tags_awaited(&self, bucket: &str, tags: &[(String, String)]) -> Result<(), Error> {
        let bucket = bucket.to_owned(); let tags = tags.to_vec();
        self.call(move |db| db.set_bucket_tags(&bucket, &tags)).await
    }
    pub async fn delete_bucket_tags_awaited(&self, bucket: &str) -> Result<(), Error> {
        let bucket = bucket.to_owned();
        self.call(move |db| db.delete_bucket_tags(&bucket)).await
    }
    pub async fn set_multipart_tagging_awaited(&self, upload_id: &str, tagging: &str) -> Result<(), Error> {
        let upload_id = upload_id.to_owned(); let tagging = tagging.to_owned();
        self.call(move |db| db.set_multipart_tagging(&upload_id, &tagging)).await
    }
    pub async fn set_bucket_acl_awaited(&self, bucket: &str, owner_id: &str, grants_json: &str) -> Result<(), Error> {
        let bucket = bucket.to_owned(); let owner_id = owner_id.to_owned(); let grants_json = grants_json.to_owned();
        self.call(move |db| db.set_bucket_acl(&bucket, &owner_id, &grants_json)).await
    }
    pub async fn set_object_acl_awaited(&self, bucket: &str, key: &str, version_id: &str, owner_id: &str, grants_json: &str) -> Result<(), Error> {
        let bucket = bucket.to_owned(); let key = key.to_owned(); let version_id = version_id.to_owned(); let owner_id = owner_id.to_owned(); let grants_json = grants_json.to_owned();
        self.call(move |db| db.set_object_acl(&bucket, &key, &version_id, &owner_id, &grants_json)).await
    }
    pub async fn set_public_access_block_awaited(&self, bucket: &str, block_public_acls: bool, ignore_public_acls: bool, block_public_policy: bool, restrict_public_buckets: bool) -> Result<(), Error> {
        let bucket = bucket.to_owned(); let block_public_acls = block_public_acls; let ignore_public_acls = ignore_public_acls; let block_public_policy = block_public_policy; let restrict_public_buckets = restrict_public_buckets;
        self.call(move |db| db.set_public_access_block(&bucket, block_public_acls, ignore_public_acls, block_public_policy, restrict_public_buckets)).await
    }
    pub async fn delete_public_access_block_awaited(&self, bucket: &str) -> Result<(), Error> {
        let bucket = bucket.to_owned();
        self.call(move |db| db.delete_public_access_block(&bucket)).await
    }
    pub async fn create_multipart_upload_awaited(&self, upload_id: &str, bucket: &str, key: &str, content_type: Option<&str>, metadata_json: &str, initiated_at: &str, checksum_algorithm: &str, checksum_type: &str, object_lock_mode: &str, object_lock_retain_until: &str, object_lock_legal_hold: &str) -> Result<(), Error> {
        let upload_id = upload_id.to_owned(); let bucket = bucket.to_owned(); let key = key.to_owned(); let content_type = content_type.map(str::to_owned); let metadata_json = metadata_json.to_owned(); let initiated_at = initiated_at.to_owned(); let checksum_algorithm = checksum_algorithm.to_owned(); let checksum_type = checksum_type.to_owned(); let object_lock_mode = object_lock_mode.to_owned(); let object_lock_retain_until = object_lock_retain_until.to_owned(); let object_lock_legal_hold = object_lock_legal_hold.to_owned();
        self.call(move |db| db.create_multipart_upload(&upload_id, &bucket, &key, content_type.as_deref(), &metadata_json, &initiated_at, &checksum_algorithm, &checksum_type, &object_lock_mode, &object_lock_retain_until, &object_lock_legal_hold)).await
    }
    pub async fn mark_multipart_completed_awaited(&self, upload_id: &str, final_etag: &str) -> Result<(), Error> {
        let upload_id = upload_id.to_owned(); let final_etag = final_etag.to_owned();
        self.call(move |db| db.mark_multipart_completed(&upload_id, &final_etag)).await
    }
    pub async fn delete_multipart_upload_awaited(&self, upload_id: &str) -> Result<(), Error> {
        let upload_id = upload_id.to_owned();
        self.call(move |db| db.delete_multipart_upload(&upload_id)).await
    }
    pub async fn delete_parts_for_upload_awaited(&self, upload_id: &str) -> Result<(), Error> {
        let upload_id = upload_id.to_owned();
        self.call(move |db| db.delete_parts_for_upload(&upload_id)).await
    }
    pub async fn set_parts_manifest_awaited(&self, bucket: &str, key: &str, manifest: &str) -> Result<(), Error> {
        let bucket = bucket.to_owned(); let key = key.to_owned(); let manifest = manifest.to_owned();
        self.call(move |db| db.set_parts_manifest(&bucket, &key, &manifest)).await
    }
    pub async fn set_bucket_object_lock_enabled_awaited(&self, bucket: &str, enabled: bool) -> Result<(), Error> {
        let bucket = bucket.to_owned(); let enabled = enabled;
        self.call(move |db| db.set_bucket_object_lock_enabled(&bucket, enabled)).await
    }
    pub async fn create_bucket_with_lock_awaited(&self, bucket: &str, lock_enabled: bool) -> Result<(), Error> {
        let bucket = bucket.to_owned(); let lock_enabled = lock_enabled;
        self.call(move |db| db.create_bucket_with_lock(&bucket, lock_enabled)).await
    }
    pub async fn put_object_lock_config_awaited(&self, bucket: &str, mode: &str, days: Option<i64>, years: Option<i64>) -> Result<(), Error> {
        let bucket = bucket.to_owned(); let mode = mode.to_owned(); let days = days; let years = years;
        self.call(move |db| db.put_object_lock_config(&bucket, &mode, days, years)).await
    }
    pub async fn put_object_lock_awaited(&self, bucket: &str, key: &str, version_id: &str, mode: Option<&str>, retain_until_date: Option<&str>, legal_hold: Option<&str>) -> Result<(), Error> {
        let bucket = bucket.to_owned(); let key = key.to_owned(); let version_id = version_id.to_owned(); let mode = mode.map(str::to_owned); let retain_until_date = retain_until_date.map(str::to_owned); let legal_hold = legal_hold.map(str::to_owned);
        self.call(move |db| db.put_object_lock(&bucket, &key, &version_id, mode.as_deref(), retain_until_date.as_deref(), legal_hold.as_deref())).await
    }
    pub async fn set_object_legal_hold_awaited(&self, bucket: &str, key: &str, version_id: &str, status: &str) -> Result<(), Error> {
        let bucket = bucket.to_owned(); let key = key.to_owned(); let version_id = version_id.to_owned(); let status = status.to_owned();
        self.call(move |db| db.set_object_legal_hold(&bucket, &key, &version_id, &status)).await
    }
}
