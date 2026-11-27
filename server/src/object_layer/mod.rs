//! The object layer: buckets, keys, versions, ETags and checksums, built on
//! the engine's stores. [`ObjectLayer`] is what every front end calls,
//! through `warp_frontend::ObjectService`, so a value is the same object
//! whichever protocol wrote it.
//!
//! It depends on the engine below and on nothing from any front end: no S3
//! handler or header code. The engine underneath should move bytes and keep
//! rows ordered and durable, knowing nothing of buckets or versions. That
//! split is the next step and not done here.

use crate::metadata::{Metadata, VersioningDeleteResult};
use crate::digest::{compute_checksum, last_modified_now, md5_etag, ChecksumAlgorithm};
use crate::identity::ADMIN_USER_ID;
use crate::service::metadata_service::MetadataService;
use crate::service::storage_service::StorageService;
use crate::service::user_context::UserContext;
use async_trait::async_trait;

pub mod metadata;
use warp_frontend::{ObjectError, ObjectResult, ObjectService, Principal};

/// The object layer every front end gets.
pub struct ObjectLayer;

/// Constant-time comparison, so a wrong guess takes as long as a right one.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn internal(e: impl std::fmt::Display) -> ObjectError {
    ObjectError::Internal(e.to_string())
}

fn db_for(who: &Principal, bucket: &str, key: &str) -> ObjectResult<MetadataService> {
    if key.is_empty() {
        return Err(ObjectError::InvalidKey("empty key".into()));
    }
    let db = MetadataService::new(&who.user_id).map_err(internal)?;
    match db.bucket_exists(bucket) {
        Ok(true) => Ok(db),
        Ok(false) => Err(ObjectError::NoSuchBucket(bucket.to_string())),
        Err(e) => Err(internal(e)),
    }
}

#[async_trait(?Send)]
impl ObjectService for ObjectLayer {
    /// The admin credentials (`WARPDRIVE_ADMIN_ACCESS_KEY`/`_SECRET_KEY`).
    fn authenticate(&self, access_key: &[u8], secret: &[u8]) -> Option<Principal> {
        match (std::env::var("WARPDRIVE_ADMIN_ACCESS_KEY"), std::env::var("WARPDRIVE_ADMIN_SECRET_KEY")) {
            (Ok(ak), Ok(sk)) if same(access_key, ak.as_bytes()) & same(secret, sk.as_bytes()) => Some(Principal { user_id: ADMIN_USER_ID.to_string() }),
            _ => None,
        }
    }

    async fn put(&self, who: &Principal, bucket: &str, key: &str, value: Vec<u8>) -> ObjectResult<()> {
        let db = db_for(who, bucket, key)?;
        let user = who.user_id.clone();
        let store = crate::storage::object_store();
        let mut extents = Vec::new();
        if !value.is_empty() {
            let written = match store.write_async(&user, bucket, value.clone()) {
                Some(fut) => fut.await,
                None => {
                    let (s, b, v, u) = (std::sync::Arc::clone(&store), bucket.to_string(), value.clone(), user.clone());
                    tokio::task::spawn_blocking(move || s.write(&u, &b, &v).map_err(|e| e.to_string())).await.map_err(internal)?
                }
            };
            extents.push(written.map_err(internal)?);
        }
        let mut meta = Metadata::from_offset_size_list(extents);
        meta.etag = Some(md5_etag(&value));
        meta.size = value.len() as u64;
        meta.content_type = Some("application/octet-stream".into());
        meta.last_modified = Some(last_modified_now());
        meta.checksum_algorithm = Some(ChecksumAlgorithm::Crc32c.as_str().to_string());
        meta.checksum_value = Some(compute_checksum(&ChecksumAlgorithm::Crc32c, &value));
        let (_, old) = db.put_object_full_async(bucket, key, meta).await.map_err(internal)?;
        if !old.is_empty() {
            db.queue_deletion(bucket, key, &old).ok();
        }
        Ok(())
    }

    async fn get(&self, who: &Principal, bucket: &str, key: &str) -> ObjectResult<Option<Vec<u8>>> {
        let db = db_for(who, bucket, key)?;
        if !db.check_key(bucket, key).map_err(internal)? {
            return Ok(None);
        }
        let meta = db.get_object_full(bucket, key).map_err(internal)?;
        let locs = crate::storage::Locators::of(&meta.chunks);
        let store = crate::storage::object_store();
        let mut out = Vec::with_capacity(meta.size as usize);
        for (off, sz) in meta.to_offset_size_list() {
            let loc = locs.find(off);
            let chunk = match store.read_located_async(&who.user_id, bucket, off, sz, loc.as_ref()) {
                Some(fut) => fut.await,
                None => {
                    let (s, b, u) = (std::sync::Arc::clone(&store), bucket.to_string(), who.user_id.clone());
                    tokio::task::spawn_blocking(move || s.read_located(&u, &b, off, sz, loc.as_ref()).map_err(|e| e.to_string()))
                        .await
                        .map_err(internal)?
                }
            };
            out.extend_from_slice(&chunk.map_err(internal)?);
        }
        Ok(Some(out))
    }

    async fn delete(&self, who: &Principal, bucket: &str, key: &str) -> ObjectResult<bool> {
        let db = db_for(who, bucket, key)?;
        if !db.check_key(bucket, key).map_err(internal)? {
            return Ok(false);
        }
        if let Some(fut) = db.delete_object_async(bucket, key) {
            fut.await.map_err(internal)?;
            return Ok(true);
        }
        if let VersioningDeleteResult::Deleted = db.delete_object_v2(bucket, key).map_err(internal)? {
            if let Ok(existing) = db.get_object_full(bucket, key) {
                let extents = existing.to_offset_size_list();
                if !extents.is_empty() {
                    db.queue_deletion(bucket, key, &extents).ok();
                }
            }
            StorageService::new().delete_object(&UserContext::with_bucket(who.user_id.clone(), bucket.into()), key).ok();
            db.delete_metadata(bucket, key).ok();
        }
        Ok(true)
    }

    fn exists(&self, who: &Principal, bucket: &str, key: &str) -> ObjectResult<bool> {
        db_for(who, bucket, key)?.check_key(bucket, key).map_err(internal)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn credentials_compare_exactly() {
        assert!(super::same(b"abc", b"abc"));
        assert!(!super::same(b"abc", b"abd"));
        assert!(!super::same(b"abc", b"abcd"));
    }
}
