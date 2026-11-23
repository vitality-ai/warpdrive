//! The Redis commands' reads and writes: the same store and metadata calls
//! as S3 PutObject, GetObject and DeleteObject, so a value is an ordinary S3
//! object either way.

use super::resp::Reply;
use crate::metadata::{Metadata, VersioningDeleteResult};
use crate::s3::auth::ADMIN_USER_ID;
use crate::s3::handlers::checksum::{compute_checksum, ChecksumAlgorithm};
use crate::s3::handlers::common::{last_modified_now, md5_etag};
use crate::service::metadata_service::MetadataService;
use crate::service::storage_service::StorageService;
use crate::service::user_context::UserContext;

/// The bucket plain (non-hash) keys live in.
pub fn default_bucket() -> String {
    std::env::var("WARPDRIVE_REDIS_BUCKET").ok().filter(|b| !b.trim().is_empty()).unwrap_or_else(|| "redis".into())
}

/// Constant-time comparison, so a wrong guess takes as long as a right one.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Whether `AUTH <user> <pass>` names the admin credentials.
pub fn credentials_match(user: &[u8], pass: &[u8]) -> bool {
    match (std::env::var("WARPDRIVE_ADMIN_ACCESS_KEY"), std::env::var("WARPDRIVE_ADMIN_SECRET_KEY")) {
        (Ok(ak), Ok(sk)) => same(user, ak.as_bytes()) & same(pass, sk.as_bytes()),
        _ => false,
    }
}

fn err(e: impl std::fmt::Display) -> Reply {
    Reply::Error(format!("ERR {}", e.to_string().replace(['\r', '\n'], " ")))
}

fn db_for(bucket: &str, key: &str) -> Result<MetadataService, Reply> {
    if key.is_empty() {
        return Err(Reply::Error("ERR empty key".into()));
    }
    let db = MetadataService::new(ADMIN_USER_ID).map_err(err)?;
    match db.bucket_exists(bucket) {
        Ok(true) => Ok(db),
        Ok(false) => Err(Reply::Error(format!("ERR no such bucket '{bucket}' (create it over S3 first)"))),
        Err(e) => Err(err(e)),
    }
}

pub fn exists(bucket: &str, key: &str) -> Result<bool, Reply> {
    db_for(bucket, key)?.check_key(bucket, key).map_err(err)
}

/// Writes `key`. With `report_new`, returns whether it is new (HSET's
/// reply needs that). Without, skips that lookup and returns false (SET
/// replies OK either way).
pub async fn put(bucket: &str, key: &str, value: Vec<u8>, report_new: bool) -> Result<bool, Reply> {
    let db = db_for(bucket, key)?;
    let existed = if report_new { db.check_key(bucket, key).map_err(err)? } else { true };
    let store = crate::storage::object_store();
    let mut extents = Vec::new();
    if !value.is_empty() {
        let written = match store.write_async(ADMIN_USER_ID, bucket, value.clone()) {
            Some(fut) => fut.await,
            None => {
                let (s, b, v) = (std::sync::Arc::clone(&store), bucket.to_string(), value.clone());
                tokio::task::spawn_blocking(move || s.write(ADMIN_USER_ID, &b, &v).map_err(|e| e.to_string())).await.map_err(err)?
            }
        };
        extents.push(written.map_err(err)?);
    }
    let mut meta = Metadata::from_offset_size_list(extents);
    meta.etag = Some(md5_etag(&value));
    meta.size = value.len() as u64;
    meta.content_type = Some("application/octet-stream".into());
    meta.last_modified = Some(last_modified_now());
    meta.checksum_algorithm = Some(ChecksumAlgorithm::Crc32c.as_str().to_string());
    meta.checksum_value = Some(compute_checksum(&ChecksumAlgorithm::Crc32c, &value));
    let (_, old) = db.put_object_full_async(bucket, key, meta).await.map_err(err)?;
    if !old.is_empty() {
        db.queue_deletion(bucket, key, &old).ok();
    }
    Ok(!existed)
}

pub async fn get(bucket: &str, key: &str) -> Result<Option<Vec<u8>>, Reply> {
    let db = db_for(bucket, key)?;
    if !db.check_key(bucket, key).map_err(err)? {
        return Ok(None);
    }
    let meta = db.get_object_full(bucket, key).map_err(err)?;
    let locs = crate::storage::Locators::of(&meta.chunks);
    let store = crate::storage::object_store();
    let mut out = Vec::with_capacity(meta.size as usize);
    for (off, sz) in meta.to_offset_size_list() {
        let loc = locs.find(off);
        let chunk = match store.read_located_async(ADMIN_USER_ID, bucket, off, sz, loc.as_ref()) {
            Some(fut) => fut.await,
            None => {
                let (s, b) = (std::sync::Arc::clone(&store), bucket.to_string());
                tokio::task::spawn_blocking(move || s.read_located(ADMIN_USER_ID, &b, off, sz, loc.as_ref()).map_err(|e| e.to_string()))
                    .await
                    .map_err(err)?
            }
        };
        out.extend_from_slice(&chunk.map_err(err)?);
    }
    Ok(Some(out))
}

/// Deletes `key` the way S3 DeleteObject does (a delete marker on a
/// versioned bucket). Returns whether it existed.
pub async fn delete(bucket: &str, key: &str) -> Result<bool, Reply> {
    let db = db_for(bucket, key)?;
    if !db.check_key(bucket, key).map_err(err)? {
        return Ok(false);
    }
    if let Some(fut) = db.delete_object_async(bucket, key) {
        fut.await.map_err(err)?;
        return Ok(true);
    }
    if let VersioningDeleteResult::Deleted = db.delete_object_v2(bucket, key).map_err(err)? {
        if let Ok(existing) = db.get_object_full(bucket, key) {
            let extents = existing.to_offset_size_list();
            if !extents.is_empty() {
                db.queue_deletion(bucket, key, &extents).ok();
            }
        }
        StorageService::new().delete_object(&UserContext::with_bucket(ADMIN_USER_ID.into(), bucket.into()), key).ok();
        db.delete_metadata(bucket, key).ok();
    }
    Ok(true)
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
