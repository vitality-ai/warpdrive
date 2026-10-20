//! Mock implementation of MetadataStorage trait for testing

use crate::metadata::{
    MetadataStorage, Metadata, ObjectId, BucketStats,
    VersioningDeleteResult, DeleteSpecificResult, VersionRow,
    DeletionEvent, MultipartUploadRow, MultipartPartRow, ObjectLockRow,
};
use actix_web::Error;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use chrono::Utc;

/// Mock implementation of MetadataStorage for testing. The methods beyond
/// basic CRUD/bucket-lifecycle (tagging, versioning, multipart, object
/// lock) exist only so this compiles against the full trait and the mock
/// backend remains usable for simple handler-level tests — they're
/// intentionally simple, not a full in-memory reimplementation of
/// SQLite's versioning/multipart semantics.
pub struct MockMetadataStore {
    data: Arc<Mutex<HashMap<String, HashMap<String, HashMap<String, Metadata>>>>>,
    buckets: Arc<Mutex<HashMap<String, HashSet<String>>>>,
    bucket_cors: Arc<Mutex<HashMap<String, String>>>,
    bucket_location: Arc<Mutex<HashMap<(String, String), String>>>,
    versioning_state: Arc<Mutex<HashMap<String, String>>>,
    object_tags: Arc<Mutex<HashMap<(String, String, String), Vec<(String, String)>>>>,
    bucket_tags: Arc<Mutex<HashMap<String, Vec<(String, String)>>>>,
    multipart_tagging: Arc<Mutex<HashMap<String, String>>>,
    multipart_uploads: Arc<Mutex<HashMap<String, MultipartUploadRow>>>,
    multipart_parts: Arc<Mutex<HashMap<String, Vec<MultipartPartRow>>>>,
    parts_manifest: Arc<Mutex<HashMap<(String, String, String), String>>>,
    deletion_queue: Arc<Mutex<Vec<DeletionEvent>>>,
    deletion_id_counter: AtomicI64,
    bucket_object_lock_enabled: Arc<Mutex<HashSet<String>>>,
    object_lock_config: Arc<Mutex<HashMap<String, (String, Option<i64>, Option<i64>)>>>,
    object_lock_rows: Arc<Mutex<HashMap<(String, String, String), ObjectLockRow>>>,
    bucket_acl: Arc<Mutex<HashMap<String, (String, String)>>>,
    object_acl: Arc<Mutex<HashMap<(String, String, String), (String, String)>>>,
    public_access_block: Arc<Mutex<HashMap<String, (bool, bool, bool, bool)>>>,
}

impl MockMetadataStore {
    pub fn new() -> Self {
        Self {
            data: Arc::new(Mutex::new(HashMap::new())),
            buckets: Arc::new(Mutex::new(HashMap::new())),
            bucket_cors: Arc::new(Mutex::new(HashMap::new())),
            bucket_location: Arc::new(Mutex::new(HashMap::new())),
            versioning_state: Arc::new(Mutex::new(HashMap::new())),
            object_tags: Arc::new(Mutex::new(HashMap::new())),
            bucket_tags: Arc::new(Mutex::new(HashMap::new())),
            multipart_tagging: Arc::new(Mutex::new(HashMap::new())),
            multipart_uploads: Arc::new(Mutex::new(HashMap::new())),
            multipart_parts: Arc::new(Mutex::new(HashMap::new())),
            parts_manifest: Arc::new(Mutex::new(HashMap::new())),
            deletion_queue: Arc::new(Mutex::new(Vec::new())),
            deletion_id_counter: AtomicI64::new(1),
            bucket_object_lock_enabled: Arc::new(Mutex::new(HashSet::new())),
            object_lock_config: Arc::new(Mutex::new(HashMap::new())),
            object_lock_rows: Arc::new(Mutex::new(HashMap::new())),
            bucket_acl: Arc::new(Mutex::new(HashMap::new())),
            object_acl: Arc::new(Mutex::new(HashMap::new())),
            public_access_block: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn clear(&self) {
        self.data.lock().unwrap().clear();
        self.buckets.lock().unwrap().clear();
    }

    pub fn user_count(&self) -> usize {
        self.data.lock().unwrap().len()
    }

    pub fn object_count(&self, user_id: &str) -> usize {
        let data = self.data.lock().unwrap();
        data.get(user_id)
            .map(|u| u.values().map(|b| b.len()).sum())
            .unwrap_or(0)
    }
}

impl Default for MockMetadataStore {
    fn default() -> Self { Self::new() }
}

impl MetadataStorage for MockMetadataStore {
    fn put_metadata(&self, user_id: &str, bucket: &str, object_id: &str, metadata: &Metadata) -> Result<(), Error> {
        let mut data = self.data.lock().unwrap();
        let bucket_data = data
            .entry(user_id.to_string()).or_default()
            .entry(bucket.to_string()).or_default();

        if bucket_data.contains_key(object_id) {
            return Err(actix_web::error::ErrorBadRequest("Key already exists"));
        }
        bucket_data.insert(object_id.to_string(), metadata.clone());
        Ok(())
    }

    fn get_metadata(&self, user_id: &str, bucket: &str, object_id: &str) -> Result<Metadata, Error> {
        let data = self.data.lock().unwrap();
        data.get(user_id)
            .and_then(|u| u.get(bucket))
            .and_then(|b| b.get(object_id))
            .cloned()
            .ok_or_else(|| actix_web::error::ErrorNotFound(format!(
                "No data found for key: {} in bucket: {}, The key does not exist", object_id, bucket
            )))
    }

    fn delete_metadata(&self, user_id: &str, bucket: &str, object_id: &str) -> Result<(), Error> {
        let mut data = self.data.lock().unwrap();
        let removed = data
            .get_mut(user_id)
            .and_then(|u| u.get_mut(bucket))
            .and_then(|b| b.remove(object_id))
            .is_some();
        if removed {
            Ok(())
        } else {
            Err(actix_web::error::ErrorNotFound(format!(
                "No data found for key: {} in bucket: {}, The key does not exist", object_id, bucket
            )))
        }
    }

    fn list_objects(&self, user_id: &str, bucket: &str) -> Result<Vec<ObjectId>, Error> {
        let data = self.data.lock().unwrap();
        let mut keys: Vec<String> = data
            .get(user_id)
            .and_then(|u| u.get(bucket))
            .map(|b| b.keys().cloned().collect())
            .unwrap_or_default();
        keys.sort();
        Ok(keys)
    }

    fn object_exists(&self, user_id: &str, bucket: &str, object_id: &str) -> Result<bool, Error> {
        let data = self.data.lock().unwrap();
        Ok(data.get(user_id)
            .and_then(|u| u.get(bucket))
            .map(|b| b.contains_key(object_id))
            .unwrap_or(false))
    }

    fn update_metadata(&self, user_id: &str, bucket: &str, object_id: &str, metadata: &Metadata) -> Result<(), Error> {
        let mut data = self.data.lock().unwrap();
        let entry = data
            .get_mut(user_id)
            .and_then(|u| u.get_mut(bucket))
            .and_then(|b| b.get_mut(object_id));
        match entry {
            Some(e) => { *e = metadata.clone(); Ok(()) }
            None => Err(actix_web::error::ErrorNotFound(format!(
                "No data found for key: {}, The key does not exist", object_id
            ))),
        }
    }

    fn update_object_id(&self, user_id: &str, bucket: &str, old_object_id: &str, new_object_id: &str) -> Result<(), Error> {
        let mut data = self.data.lock().unwrap();
        let metadata = data
            .get_mut(user_id)
            .and_then(|u| u.get_mut(bucket))
            .and_then(|b| b.remove(old_object_id));
        match metadata {
            Some(m) => {
                data.get_mut(user_id).unwrap().get_mut(bucket).unwrap()
                    .insert(new_object_id.to_string(), m);
                Ok(())
            }
            None => Err(actix_web::error::ErrorNotFound(format!(
                "No data found for key: {}, The key does not exist", old_object_id
            ))),
        }
    }

    fn queue_deletion(&self, user_id: &str, bucket: &str, key: &str, offset_size_list: &[(u64, u64)]) -> Result<(), Error> {
        let id = self.deletion_id_counter.fetch_add(1, Ordering::Relaxed);
        self.deletion_queue.lock().unwrap().push(DeletionEvent {
            id,
            user_id: user_id.to_string(),
            bucket: bucket.to_string(),
            key: key.to_string(),
            offset_size_list: offset_size_list.to_vec(),
            created_at: Utc::now().to_rfc3339(),
        });
        Ok(())
    }

    fn list_buckets_with_stats(&self, user_id: &str) -> Result<Vec<BucketStats>, Error> {
        let data = self.data.lock().unwrap();
        let buckets = self.buckets.lock().unwrap();

        // All registered buckets, including empty ones
        let registered = buckets.get(user_id).cloned().unwrap_or_default();
        let now = Utc::now().format("%Y-%m-%dT%H:%M:%S.000Z").to_string();
        let mut stats: Vec<BucketStats> = registered.iter().map(|name| {
            let (object_count, total_size) = data
                .get(user_id)
                .and_then(|u| u.get(name))
                .map(|b| {
                    let count = b.len() as u64;
                    let size: u64 = b.values().map(|m| m.size).sum();
                    (count, size)
                })
                .unwrap_or((0, 0));
            BucketStats { name: name.clone(), created_at: now.clone(), object_count, total_size }
        }).collect();
        stats.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(stats)
    }

    fn create_bucket(&self, user_id: &str, bucket: &str) -> Result<(), Error> {
        self.buckets.lock().unwrap()
            .entry(user_id.to_string()).or_default()
            .insert(bucket.to_string());
        Ok(())
    }

    fn delete_bucket(&self, user_id: &str, bucket: &str) -> Result<(), Error> {
        self.buckets.lock().unwrap()
            .entry(user_id.to_string()).or_default()
            .remove(bucket);
        Ok(())
    }

    fn bucket_exists(&self, user_id: &str, bucket: &str) -> Result<bool, Error> {
        Ok(self.buckets.lock().unwrap()
            .get(user_id)
            .map(|s| s.contains(bucket))
            .unwrap_or(false))
    }

    fn list_all_buckets_for_user(&self, user_id: &str) -> Result<Vec<String>, Error> {
        let buckets = self.buckets.lock().unwrap();
        let mut names: Vec<String> = buckets.get(user_id)
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default();
        names.sort();
        Ok(names)
    }

    fn bucket_object_stats(&self, user_id: &str, bucket: &str) -> Result<(u64, u64), Error> {
        let data = self.data.lock().unwrap();
        let (count, bytes) = data.get(user_id)
            .and_then(|u| u.get(bucket))
            .map(|b| {
                let count = b.len() as u64;
                let bytes: u64 = b.values().map(|m| m.size).sum();
                (count, bytes)
            })
            .unwrap_or((0, 0));
        Ok((count, bytes))
    }

    fn put_object_v2(&self, user_id: &str, bucket: &str, key: &str, metadata: &Metadata) -> Result<(Option<String>, Vec<(u64, u64)>), Error> {
        let mut data = self.data.lock().unwrap();
        let old = data
            .entry(user_id.to_string()).or_default()
            .entry(bucket.to_string()).or_default()
            .insert(key.to_string(), metadata.clone());
        let old_extents = old.map(|m| m.to_offset_size_list()).unwrap_or_default();
        Ok((None, old_extents))
    }

    fn get_pending_deletions(&self, limit: i32) -> Result<Vec<DeletionEvent>, Error> {
        let q = self.deletion_queue.lock().unwrap();
        Ok(q.iter().take(limit.max(0) as usize).cloned().collect())
    }

    fn mark_deletion_processed(&self, id: i64) -> Result<(), Error> {
        self.deletion_queue.lock().unwrap().retain(|e| e.id != id);
        Ok(())
    }

    fn cleanup_old_deletions(&self) -> Result<usize, Error> {
        Ok(0)
    }

    fn set_bucket_cors(&self, bucket: &str, cors_xml: &str) -> Result<(), Error> {
        self.bucket_cors.lock().unwrap().insert(bucket.to_string(), cors_xml.to_string());
        Ok(())
    }

    fn get_bucket_cors(&self, bucket: &str) -> Result<Option<String>, Error> {
        Ok(self.bucket_cors.lock().unwrap().get(bucket).cloned())
    }

    fn delete_bucket_cors(&self, bucket: &str) -> Result<(), Error> {
        self.bucket_cors.lock().unwrap().remove(bucket);
        Ok(())
    }

    fn set_bucket_location(&self, user_id: &str, bucket: &str, location: &str) -> Result<(), Error> {
        self.bucket_location.lock().unwrap().insert((user_id.to_string(), bucket.to_string()), location.to_string());
        Ok(())
    }

    fn get_bucket_location(&self, user_id: &str, bucket: &str) -> Result<String, Error> {
        Ok(self.bucket_location.lock().unwrap().get(&(user_id.to_string(), bucket.to_string())).cloned().unwrap_or_default())
    }

    fn get_versioning_state(&self, bucket: &str) -> Result<String, Error> {
        Ok(self.versioning_state.lock().unwrap().get(bucket).cloned().unwrap_or_else(|| "disabled".to_string()))
    }

    fn set_versioning_state(&self, _user_id: &str, bucket: &str, state: &str) -> Result<(), Error> {
        self.versioning_state.lock().unwrap().insert(bucket.to_string(), state.to_string());
        Ok(())
    }

    fn delete_object_v2(&self, user_id: &str, bucket: &str, key: &str) -> Result<VersioningDeleteResult, Error> {
        self.data.lock().unwrap()
            .get_mut(user_id)
            .and_then(|u| u.get_mut(bucket))
            .and_then(|b| b.remove(key));
        Ok(VersioningDeleteResult::Deleted)
    }

    fn delete_specific_version(&self, _user_id: &str, _bucket: &str, _key: &str, version_id: &str) -> Result<DeleteSpecificResult, Error> {
        // Mock doesn't track multiple versions per key (see struct doc).
        Ok(DeleteSpecificResult { found: false, was_delete_marker: false, version_id: version_id.to_string() })
    }

    fn get_object_version(&self, user_id: &str, bucket: &str, key: &str, _version_id: &str) -> Result<Metadata, Error> {
        self.get_metadata(user_id, bucket, key)
    }

    fn get_latest_last_modified(&self, user_id: &str, bucket: &str, key: &str) -> Result<Option<String>, Error> {
        Ok(self.data.lock().unwrap()
            .get(user_id)
            .and_then(|u| u.get(bucket))
            .and_then(|b| b.get(key))
            .and_then(|m| m.last_modified.clone()))
    }

    fn get_latest_version_marker_info(&self, user_id: &str, bucket: &str, key: &str) -> Result<Option<(String, bool)>, Error> {
        Ok(self.data.lock().unwrap()
            .get(user_id)
            .and_then(|u| u.get(bucket))
            .and_then(|b| b.get(key))
            .map(|m| (m.version_id.clone().unwrap_or_default(), m.is_delete_marker)))
    }

    fn list_object_versions_full(
        &self, _user_id: &str, _bucket: &str,
        _prefix: &str, _key_marker: &str, _version_id_marker: &str, _max_keys: usize,
    ) -> Result<(Vec<VersionRow>, bool, String, String), Error> {
        // Mock doesn't track version history (see struct doc).
        Ok((Vec::new(), false, String::new(), String::new()))
    }

    fn set_object_tags(&self, user_id: &str, bucket: &str, key: &str, tags: &[(String, String)]) -> Result<(), Error> {
        self.object_tags.lock().unwrap().insert(
            (user_id.to_string(), bucket.to_string(), key.to_string()),
            tags.to_vec(),
        );
        Ok(())
    }

    fn get_object_tags(&self, user_id: &str, bucket: &str, key: &str) -> Result<Vec<(String, String)>, Error> {
        Ok(self.object_tags.lock().unwrap()
            .get(&(user_id.to_string(), bucket.to_string(), key.to_string()))
            .cloned()
            .unwrap_or_default())
    }

    fn delete_object_tags(&self, user_id: &str, bucket: &str, key: &str) -> Result<(), Error> {
        self.object_tags.lock().unwrap().remove(&(user_id.to_string(), bucket.to_string(), key.to_string()));
        Ok(())
    }

    fn get_object_tag_count(&self, user_id: &str, bucket: &str, key: &str) -> Result<i64, Error> {
        Ok(self.get_object_tags(user_id, bucket, key)?.len() as i64)
    }

    fn set_bucket_tags(&self, bucket: &str, tags: &[(String, String)]) -> Result<(), Error> {
        self.bucket_tags.lock().unwrap().insert(bucket.to_string(), tags.to_vec());
        Ok(())
    }

    fn get_bucket_tags(&self, bucket: &str) -> Result<Vec<(String, String)>, Error> {
        Ok(self.bucket_tags.lock().unwrap().get(bucket).cloned().unwrap_or_default())
    }

    fn delete_bucket_tags(&self, bucket: &str) -> Result<(), Error> {
        self.bucket_tags.lock().unwrap().remove(bucket);
        Ok(())
    }

    fn set_multipart_tagging(&self, upload_id: &str, tagging: &str) -> Result<(), Error> {
        self.multipart_tagging.lock().unwrap().insert(upload_id.to_string(), tagging.to_string());
        Ok(())
    }

    fn get_multipart_tagging(&self, upload_id: &str) -> Result<String, Error> {
        Ok(self.multipart_tagging.lock().unwrap().get(upload_id).cloned().unwrap_or_default())
    }

    fn set_bucket_acl(&self, bucket: &str, owner_id: &str, grants_json: &str) -> Result<(), Error> {
        self.bucket_acl.lock().unwrap().insert(bucket.to_string(), (owner_id.to_string(), grants_json.to_string()));
        Ok(())
    }

    fn get_bucket_acl(&self, bucket: &str) -> Result<Option<(String, String)>, Error> {
        Ok(self.bucket_acl.lock().unwrap().get(bucket).cloned())
    }

    fn set_object_acl(&self, bucket: &str, key: &str, version_id: &str, owner_id: &str, grants_json: &str) -> Result<(), Error> {
        self.object_acl.lock().unwrap().insert(
            (bucket.to_string(), key.to_string(), version_id.to_string()),
            (owner_id.to_string(), grants_json.to_string()),
        );
        Ok(())
    }

    fn get_object_acl(&self, bucket: &str, key: &str, version_id: &str) -> Result<Option<(String, String)>, Error> {
        Ok(self.object_acl.lock().unwrap().get(&(bucket.to_string(), key.to_string(), version_id.to_string())).cloned())
    }

    fn set_public_access_block(&self, bucket: &str, block_public_acls: bool, ignore_public_acls: bool, block_public_policy: bool, restrict_public_buckets: bool) -> Result<(), Error> {
        self.public_access_block.lock().unwrap().insert(
            bucket.to_string(),
            (block_public_acls, ignore_public_acls, block_public_policy, restrict_public_buckets),
        );
        Ok(())
    }

    fn get_public_access_block(&self, bucket: &str) -> Result<Option<(bool, bool, bool, bool)>, Error> {
        Ok(self.public_access_block.lock().unwrap().get(bucket).cloned())
    }

    fn delete_public_access_block(&self, bucket: &str) -> Result<(), Error> {
        self.public_access_block.lock().unwrap().remove(bucket);
        Ok(())
    }

    fn create_multipart_upload(
        &self, upload_id: &str, user_id: &str, bucket: &str, key: &str,
        content_type: Option<&str>, metadata_json: &str, initiated_at: &str,
        checksum_algorithm: &str, checksum_type: &str,
        object_lock_mode: &str, object_lock_retain_until: &str, object_lock_legal_hold: &str,
    ) -> Result<(), Error> {
        self.multipart_uploads.lock().unwrap().entry(upload_id.to_string()).or_insert_with(|| MultipartUploadRow {
            upload_id: upload_id.to_string(),
            user_id: user_id.to_string(),
            bucket: bucket.to_string(),
            key: key.to_string(),
            content_type: content_type.map(|s| s.to_string()),
            metadata_json: metadata_json.to_string(),
            initiated_at: initiated_at.to_string(),
            status: "in_progress".to_string(),
            final_etag: None,
            checksum_algorithm: checksum_algorithm.to_string(),
            checksum_type: checksum_type.to_string(),
            object_lock_mode: object_lock_mode.to_string(),
            object_lock_retain_until: object_lock_retain_until.to_string(),
            object_lock_legal_hold: object_lock_legal_hold.to_string(),
        });
        Ok(())
    }

    fn get_multipart_upload(&self, upload_id: &str) -> Result<Option<MultipartUploadRow>, Error> {
        Ok(self.multipart_uploads.lock().unwrap().get(upload_id).cloned())
    }

    fn mark_multipart_completed(&self, upload_id: &str, final_etag: &str) -> Result<(), Error> {
        if let Some(row) = self.multipart_uploads.lock().unwrap().get_mut(upload_id) {
            row.status = "completed".to_string();
            row.final_etag = Some(final_etag.to_string());
        }
        Ok(())
    }

    fn delete_multipart_upload(&self, upload_id: &str) -> Result<(), Error> {
        self.multipart_uploads.lock().unwrap().remove(upload_id);
        self.multipart_parts.lock().unwrap().remove(upload_id);
        Ok(())
    }

    fn delete_completed_uploads_for_key(&self, bucket: &str, key: &str) -> Result<(), Error> {
        self.multipart_uploads.lock().unwrap().retain(|_, row| {
            !(row.bucket == bucket && row.key == key && row.status == "completed")
        });
        Ok(())
    }

    fn list_bucket_multipart_uploads(&self, bucket: &str) -> Result<Vec<MultipartUploadRow>, Error> {
        let mut rows: Vec<MultipartUploadRow> = self.multipart_uploads.lock().unwrap()
            .values()
            .filter(|row| row.bucket == bucket && row.status == "in_progress")
            .cloned()
            .collect();
        rows.sort_by(|a, b| (a.key.as_str(), a.initiated_at.as_str()).cmp(&(b.key.as_str(), b.initiated_at.as_str())));
        Ok(rows)
    }

    fn upsert_multipart_part(
        &self, upload_id: &str, part_number: i32, etag: &str, size: u64, extents_blob: &[u8],
        checksum_value: &str,
    ) -> Result<(), Error> {
        let mut parts = self.multipart_parts.lock().unwrap();
        let entry = parts.entry(upload_id.to_string()).or_default();
        entry.retain(|p| p.part_number != part_number);
        entry.push(MultipartPartRow {
            part_number,
            etag: etag.to_string(),
            size,
            extents_blob: extents_blob.to_vec(),
            checksum_value: checksum_value.to_string(),
        });
        Ok(())
    }

    fn list_multipart_parts(&self, upload_id: &str) -> Result<Vec<MultipartPartRow>, Error> {
        let mut parts = self.multipart_parts.lock().unwrap().get(upload_id).cloned().unwrap_or_default();
        parts.sort_by_key(|p| p.part_number);
        Ok(parts)
    }

    fn delete_parts_for_upload(&self, upload_id: &str) -> Result<(), Error> {
        self.multipart_parts.lock().unwrap().remove(upload_id);
        Ok(())
    }

    fn get_parts_manifest(&self, user_id: &str, bucket: &str, key: &str) -> Result<Option<String>, Error> {
        Ok(self.parts_manifest.lock().unwrap().get(&(user_id.to_string(), bucket.to_string(), key.to_string())).cloned())
    }

    fn set_parts_manifest(&self, user_id: &str, bucket: &str, key: &str, manifest: &str) -> Result<(), Error> {
        self.parts_manifest.lock().unwrap().insert(
            (user_id.to_string(), bucket.to_string(), key.to_string()),
            manifest.to_string(),
        );
        Ok(())
    }

    fn get_bucket_object_lock_enabled(&self, bucket: &str) -> Result<bool, Error> {
        Ok(self.bucket_object_lock_enabled.lock().unwrap().contains(bucket))
    }

    fn set_bucket_object_lock_enabled(&self, bucket: &str, enabled: bool) -> Result<(), Error> {
        let mut set = self.bucket_object_lock_enabled.lock().unwrap();
        if enabled { set.insert(bucket.to_string()); } else { set.remove(bucket); }
        Ok(())
    }

    fn create_bucket_with_lock(&self, user_id: &str, bucket: &str, lock_enabled: bool) -> Result<(), Error> {
        self.create_bucket(user_id, bucket)?;
        if lock_enabled {
            self.bucket_object_lock_enabled.lock().unwrap().insert(bucket.to_string());
        }
        Ok(())
    }

    fn get_object_lock_config(&self, bucket: &str) -> Result<Option<(String, Option<i64>, Option<i64>)>, Error> {
        Ok(self.object_lock_config.lock().unwrap().get(bucket).cloned())
    }

    fn put_object_lock_config(&self, bucket: &str, mode: &str, days: Option<i64>, years: Option<i64>) -> Result<(), Error> {
        self.object_lock_config.lock().unwrap().insert(bucket.to_string(), (mode.to_string(), days, years));
        Ok(())
    }

    fn get_object_lock(&self, bucket: &str, key: &str, version_id: &str) -> Result<Option<ObjectLockRow>, Error> {
        Ok(self.object_lock_rows.lock().unwrap()
            .get(&(bucket.to_string(), key.to_string(), version_id.to_string()))
            .cloned())
    }

    fn put_object_lock(
        &self, bucket: &str, key: &str, version_id: &str,
        mode: Option<&str>, retain_until_date: Option<&str>, legal_hold: Option<&str>,
    ) -> Result<(), Error> {
        let mut rows = self.object_lock_rows.lock().unwrap();
        let entry = rows.entry((bucket.to_string(), key.to_string(), version_id.to_string()))
            .or_insert_with(|| ObjectLockRow { mode: None, retain_until_date: None, legal_hold: "OFF".to_string() });
        if let Some(m) = mode { entry.mode = Some(m.to_string()); }
        if let Some(r) = retain_until_date { entry.retain_until_date = Some(r.to_string()); }
        if let Some(l) = legal_hold { entry.legal_hold = l.to_string(); }
        Ok(())
    }

    fn set_object_legal_hold(&self, bucket: &str, key: &str, version_id: &str, status: &str) -> Result<(), Error> {
        let mut rows = self.object_lock_rows.lock().unwrap();
        let entry = rows.entry((bucket.to_string(), key.to_string(), version_id.to_string()))
            .or_insert_with(|| ObjectLockRow { mode: None, retain_until_date: None, legal_hold: "OFF".to_string() });
        entry.legal_hold = status.to_string();
        Ok(())
    }

    fn check_object_lock_protection(
        &self, bucket: &str, key: &str, version_id: &str, bypass_governance: bool,
    ) -> Result<(bool, bool), Error> {
        let row = match self.get_object_lock(bucket, key, version_id)? {
            Some(r) => r,
            None => return Ok((false, false)),
        };
        let legal_hold_blocked = row.legal_hold == "ON";
        let retention_blocked = match (&row.mode, &row.retain_until_date) {
            (Some(mode), Some(until)) => {
                let now = Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
                let still_locked = until.as_str() > now.as_str();
                still_locked && !(bypass_governance && mode == "GOVERNANCE")
            }
            _ => false,
        };
        Ok((retention_blocked, legal_hold_blocked))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mock_metadata_store_basic_operations() {
        let store = MockMetadataStore::new();
        let user_id = "test_user_mock";
        let object_id = "test_object_mock";

        store.create_bucket(user_id, "default").unwrap();

        let mut metadata = Metadata::from_offset_size_list(vec![(100, 200), (300, 400)]);
        metadata.properties.insert("test_prop".to_string(), "test_value".to_string());

        assert_eq!(store.user_count(), 0);
        store.put_metadata(user_id, "default", object_id, &metadata).unwrap();
        assert_eq!(store.object_count(user_id), 1);

        assert!(store.put_metadata(user_id, "default", object_id, &metadata).is_err());

        assert!(store.object_exists(user_id, "default", object_id).unwrap());
        assert!(!store.object_exists(user_id, "default", "nonexistent").unwrap());

        let retrieved = store.get_metadata(user_id, "default", object_id).unwrap();
        assert_eq!(retrieved.to_offset_size_list(), vec![(100, 200), (300, 400)]);
        assert_eq!(retrieved.properties.get("test_prop"), Some(&"test_value".to_string()));

        let objects = store.list_objects(user_id, "default").unwrap();
        assert!(objects.contains(&object_id.to_string()));

        let new_metadata = Metadata::from_offset_size_list(vec![(500, 600)]);
        store.update_metadata(user_id, "default", object_id, &new_metadata).unwrap();
        let updated = store.get_metadata(user_id, "default", object_id).unwrap();
        assert_eq!(updated.to_offset_size_list(), vec![(500, 600)]);

        let new_object_id = "new_test_object_mock";
        store.update_object_id(user_id, "default", object_id, new_object_id).unwrap();
        assert!(!store.object_exists(user_id, "default", object_id).unwrap());
        assert!(store.object_exists(user_id, "default", new_object_id).unwrap());

        store.delete_metadata(user_id, "default", new_object_id).unwrap();
        assert_eq!(store.object_count(user_id), 0);

        store.put_metadata(user_id, "default", object_id, &metadata).unwrap();
        store.clear();
        assert_eq!(store.user_count(), 0);
    }

    #[test]
    fn test_mock_bucket_lifecycle() {
        let store = MockMetadataStore::new();
        let user_id = "bucket_test_user";

        assert!(!store.bucket_exists(user_id, "my-bucket").unwrap());
        store.create_bucket(user_id, "my-bucket").unwrap();
        assert!(store.bucket_exists(user_id, "my-bucket").unwrap());

        let names = store.list_all_buckets_for_user(user_id).unwrap();
        assert!(names.contains(&"my-bucket".to_string()));

        store.delete_bucket(user_id, "my-bucket").unwrap();
        assert!(!store.bucket_exists(user_id, "my-bucket").unwrap());
    }
}
