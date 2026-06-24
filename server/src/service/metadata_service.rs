//! Metadata service layer bridging handlers with the MetadataStorage trait

use crate::metadata::{MetadataStorage, Metadata, BucketStats, config::MetadataConfig};
use std::sync::Arc;
use actix_web::Error;
use lazy_static::lazy_static;

lazy_static! {
    static ref METADATA_STORE: Arc<dyn MetadataStorage> = {
        let config = MetadataConfig::from_env();
        config.create_store()
    };
}

pub struct MetadataService {
    user: String,
}

impl MetadataService {
    pub fn new(user: &str) -> Result<Self, Error> {
        Ok(Self { user: user.to_string() })
    }

    // --- Object existence / key checks ---

    pub fn check_key(&self, bucket: &str, key: &str) -> Result<bool, Error> {
        METADATA_STORE.object_exists(&self.user, bucket, key)
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
    pub fn put_object_full(&self, bucket: &str, key: &str, metadata: Metadata) -> Result<(), Error> {
        METADATA_STORE.put_metadata(&self.user, bucket, key, &metadata)
    }

    /// Read a fully-populated Metadata object (S3 GET / HEAD path).
    pub fn get_object_full(&self, bucket: &str, key: &str) -> Result<Metadata, Error> {
        METADATA_STORE.get_metadata(&self.user, bucket, key)
    }

    // --- Legacy bytes-based path (old native API and internal use) ---

    pub fn write_metadata(&self, bucket: &str, key: &str, offset_size_bytes: &[u8]) -> Result<(), Error> {
        use crate::util::serializer::deserialize_offset_size;
        let offset_size_list = deserialize_offset_size(offset_size_bytes)?;
        let metadata = Metadata::from_offset_size_list(offset_size_list);
        METADATA_STORE.put_metadata(&self.user, bucket, key, &metadata)
    }

    pub fn read_metadata(&self, bucket: &str, key: &str) -> Result<Vec<u8>, Error> {
        use crate::util::serializer::serialize_offset_size;
        let metadata = METADATA_STORE.get_metadata(&self.user, bucket, key)?;
        let offset_size_list = metadata.to_offset_size_list();
        serialize_offset_size(&offset_size_list)
    }

    pub fn delete_metadata(&self, bucket: &str, key: &str) -> Result<(), Error> {
        METADATA_STORE.delete_metadata(&self.user, bucket, key)
    }

    pub fn rename_key(&self, bucket: &str, old_key: &str, new_key: &str) -> Result<(), Error> {
        METADATA_STORE.update_object_id(&self.user, bucket, old_key, new_key)
    }

    pub fn update_metadata(&self, bucket: &str, key: &str, offset_size_bytes: &[u8]) -> Result<(), Error> {
        use crate::util::serializer::deserialize_offset_size;
        let offset_size_list = deserialize_offset_size(offset_size_bytes)?;
        let metadata = Metadata::from_offset_size_list(offset_size_list);
        METADATA_STORE.update_metadata(&self.user, bucket, key, &metadata)
    }

    pub fn append_metadata(&self, bucket: &str, key: &str, offset_size_bytes: &[u8]) -> Result<(), Error> {
        self.update_metadata(bucket, key, offset_size_bytes)
    }

    pub fn list_objects(&self, bucket: &str) -> Result<Vec<String>, Error> {
        METADATA_STORE.list_objects(&self.user, bucket)
    }

    // --- Bucket management ---

    pub fn create_bucket(&self, bucket: &str) -> Result<(), Error> {
        METADATA_STORE.create_bucket(&self.user, bucket)
    }

    pub fn delete_bucket(&self, bucket: &str) -> Result<(), Error> {
        METADATA_STORE.delete_bucket(&self.user, bucket)
    }

    pub fn bucket_exists(&self, bucket: &str) -> Result<bool, Error> {
        METADATA_STORE.bucket_exists(&self.user, bucket)
    }

    pub fn list_all_buckets(&self) -> Result<Vec<String>, Error> {
        METADATA_STORE.list_all_buckets_for_user(&self.user)
    }

    // --- Stats ---

    pub fn list_buckets_with_stats(&self) -> Result<Vec<BucketStats>, Error> {
        METADATA_STORE.list_buckets_with_stats(&self.user)
    }

    pub fn bucket_object_stats(&self, bucket: &str) -> Result<(u64, u64), Error> {
        METADATA_STORE.bucket_object_stats(&self.user, bucket)
    }

    // --- Deletion WAL ---

    pub fn queue_deletion(&self, bucket: &str, key: &str, offset_size_list: &[(u64, u64)]) -> Result<(), Error> {
        METADATA_STORE.queue_deletion(&self.user, bucket, key, offset_size_list)
    }

    pub fn get_pending_deletions(&self, limit: i32) -> Result<Vec<crate::metadata::sqlite_store::DeletionEvent>, Error> {
        use crate::metadata::sqlite_store::SQLiteMetadataStore;
        SQLiteMetadataStore::new().get_pending_deletions(limit)
    }

    pub fn mark_deletion_processed(&self, id: i64) -> Result<(), Error> {
        use crate::metadata::sqlite_store::SQLiteMetadataStore;
        SQLiteMetadataStore::new().mark_deletion_processed(id)
    }

    pub fn cleanup_old_deletions(&self) -> Result<usize, Error> {
        use crate::metadata::sqlite_store::SQLiteMetadataStore;
        SQLiteMetadataStore::new().cleanup_old_deletions()
    }

    // --- CORS ---

    pub fn set_bucket_cors(&self, bucket: &str, cors_xml: &str) -> Result<(), Error> {
        use crate::metadata::sqlite_store::SQLiteMetadataStore;
        SQLiteMetadataStore::new().set_bucket_cors(bucket, cors_xml)
    }

    pub fn get_bucket_cors(&self, bucket: &str) -> Result<Option<String>, Error> {
        use crate::metadata::sqlite_store::SQLiteMetadataStore;
        SQLiteMetadataStore::new().get_bucket_cors(bucket)
    }

    pub fn delete_bucket_cors(&self, bucket: &str) -> Result<(), Error> {
        use crate::metadata::sqlite_store::SQLiteMetadataStore;
        SQLiteMetadataStore::new().delete_bucket_cors(bucket)
    }

    // --- Bucket location ---

    pub fn set_bucket_location(&self, bucket: &str, location: &str) -> Result<(), Error> {
        use crate::metadata::sqlite_store::SQLiteMetadataStore;
        SQLiteMetadataStore::new().set_bucket_location(&self.user, bucket, location)
    }

    pub fn get_bucket_location(&self, bucket: &str) -> Result<String, Error> {
        use crate::metadata::sqlite_store::SQLiteMetadataStore;
        SQLiteMetadataStore::new().get_bucket_location(&self.user, bucket)
    }

    // --- Tagging ---

    pub fn set_object_tags(&self, bucket: &str, key: &str, tags: &[(String, String)]) -> Result<(), Error> {
        use crate::metadata::sqlite_store::SQLiteMetadataStore;
        SQLiteMetadataStore::new().set_object_tags(&self.user, bucket, key, tags)
    }

    pub fn get_object_tags(&self, bucket: &str, key: &str) -> Result<Vec<(String, String)>, Error> {
        use crate::metadata::sqlite_store::SQLiteMetadataStore;
        SQLiteMetadataStore::new().get_object_tags(&self.user, bucket, key)
    }

    pub fn delete_object_tags(&self, bucket: &str, key: &str) -> Result<(), Error> {
        use crate::metadata::sqlite_store::SQLiteMetadataStore;
        SQLiteMetadataStore::new().delete_object_tags(&self.user, bucket, key)
    }

    pub fn get_object_tag_count(&self, bucket: &str, key: &str) -> Result<i64, Error> {
        use crate::metadata::sqlite_store::SQLiteMetadataStore;
        SQLiteMetadataStore::new().get_object_tag_count(&self.user, bucket, key)
    }

    pub fn set_bucket_tags(&self, bucket: &str, tags: &[(String, String)]) -> Result<(), Error> {
        use crate::metadata::sqlite_store::SQLiteMetadataStore;
        SQLiteMetadataStore::new().set_bucket_tags(bucket, tags)
    }

    pub fn get_bucket_tags(&self, bucket: &str) -> Result<Vec<(String, String)>, Error> {
        use crate::metadata::sqlite_store::SQLiteMetadataStore;
        SQLiteMetadataStore::new().get_bucket_tags(bucket)
    }

    pub fn delete_bucket_tags(&self, bucket: &str) -> Result<(), Error> {
        use crate::metadata::sqlite_store::SQLiteMetadataStore;
        SQLiteMetadataStore::new().delete_bucket_tags(bucket)
    }

    pub fn set_multipart_tagging(&self, upload_id: &str, tagging: &str) -> Result<(), Error> {
        use crate::metadata::sqlite_store::SQLiteMetadataStore;
        SQLiteMetadataStore::new().set_multipart_tagging(upload_id, tagging)
    }

    pub fn get_multipart_tagging(&self, upload_id: &str) -> Result<String, Error> {
        use crate::metadata::sqlite_store::SQLiteMetadataStore;
        SQLiteMetadataStore::new().get_multipart_tagging(upload_id)
    }

    // --- Multipart upload management ---

    pub fn create_multipart_upload(
        &self, upload_id: &str, bucket: &str, key: &str,
        content_type: Option<&str>, metadata_json: &str, initiated_at: &str,
    ) -> Result<(), Error> {
        use crate::metadata::sqlite_store::SQLiteMetadataStore;
        SQLiteMetadataStore::new().create_multipart_upload(
            upload_id, &self.user, bucket, key, content_type, metadata_json, initiated_at,
        )
    }

    pub fn get_multipart_upload(&self, upload_id: &str)
        -> Result<Option<crate::metadata::sqlite_store::MultipartUploadRow>, Error>
    {
        use crate::metadata::sqlite_store::SQLiteMetadataStore;
        SQLiteMetadataStore::new().get_multipart_upload(upload_id)
    }

    pub fn mark_multipart_completed(&self, upload_id: &str, final_etag: &str) -> Result<(), Error> {
        use crate::metadata::sqlite_store::SQLiteMetadataStore;
        SQLiteMetadataStore::new().mark_multipart_completed(upload_id, final_etag)
    }

    pub fn delete_multipart_upload(&self, upload_id: &str) -> Result<(), Error> {
        use crate::metadata::sqlite_store::SQLiteMetadataStore;
        SQLiteMetadataStore::new().delete_multipart_upload(upload_id)
    }

    pub fn delete_completed_uploads_for_key(&self, bucket: &str, key: &str) -> Result<(), Error> {
        use crate::metadata::sqlite_store::SQLiteMetadataStore;
        SQLiteMetadataStore::new().delete_completed_uploads_for_key(bucket, key)
    }

    pub fn list_multipart_uploads_for_bucket(&self, bucket: &str)
        -> Result<Vec<crate::metadata::sqlite_store::MultipartUploadRow>, Error>
    {
        use crate::metadata::sqlite_store::SQLiteMetadataStore;
        SQLiteMetadataStore::new().list_bucket_multipart_uploads(bucket)
    }

    pub fn upsert_multipart_part(
        &self, upload_id: &str, part_number: i32, etag: &str, size: u64, extents_blob: &[u8],
    ) -> Result<(), Error> {
        use crate::metadata::sqlite_store::SQLiteMetadataStore;
        SQLiteMetadataStore::new().upsert_multipart_part(upload_id, part_number, etag, size, extents_blob)
    }

    pub fn list_multipart_parts(&self, upload_id: &str)
        -> Result<Vec<crate::metadata::sqlite_store::MultipartPartRow>, Error>
    {
        use crate::metadata::sqlite_store::SQLiteMetadataStore;
        SQLiteMetadataStore::new().list_multipart_parts(upload_id)
    }

    pub fn delete_parts_for_upload(&self, upload_id: &str) -> Result<(), Error> {
        use crate::metadata::sqlite_store::SQLiteMetadataStore;
        SQLiteMetadataStore::new().delete_parts_for_upload(upload_id)
    }

    pub fn get_parts_manifest(&self, bucket: &str, key: &str) -> Result<Option<String>, Error> {
        use crate::metadata::sqlite_store::SQLiteMetadataStore;
        SQLiteMetadataStore::new().get_parts_manifest(&self.user, bucket, key)
    }

    pub fn set_parts_manifest(&self, bucket: &str, key: &str, manifest: &str) -> Result<(), Error> {
        use crate::metadata::sqlite_store::SQLiteMetadataStore;
        SQLiteMetadataStore::new().set_parts_manifest(&self.user, bucket, key, manifest)
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
