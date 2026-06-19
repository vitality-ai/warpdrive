//! SQLite implementation of MetadataStorage trait

use crate::metadata::{MetadataStorage, Metadata, ObjectId, BucketStats};
use crate::util::serializer::serialize_offset_size;
use std::sync::Mutex;
use rusqlite::{params, Connection};
use std::sync::Arc;
use log::{warn, info, error};
use actix_web::Error;
use lazy_static::lazy_static;
use std::env;
use std::path::{Path, PathBuf};

fn get_db_path() -> PathBuf {
    match env::var("DB_FILE") {
        Ok(path) => {
            info!("Using database path from environment: {}", path);
            PathBuf::from(path)
        }
        Err(_) => {
            warn!("Metadata database location not defined in environment");
            let default_path = Path::new("metadata").join("metadata.sqlite");
            if let Some(parent) = default_path.parent() {
                std::fs::create_dir_all(parent).expect("Failed to create metadata directory");
            }
            info!("Using default database path: {}", default_path.display());
            default_path
        }
    }
}

lazy_static! {
    static ref DB_CONN: Arc<Mutex<Connection>> = {
        let db_path = get_db_path();
        let conn = Connection::open(&db_path).expect("Failed to open the database");

        // Enable WAL mode for better write concurrency
        conn.execute_batch("PRAGMA journal_mode=WAL;").ok();

        // Core object table
        conn.execute(
            "CREATE TABLE IF NOT EXISTS haystack (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                user TEXT NOT NULL,
                bucket TEXT NOT NULL DEFAULT 'default',
                key TEXT NOT NULL,
                offset_size_list BLOB,
                UNIQUE(user, bucket, key)
            )",
            [],
        ).expect("Failed to create haystack table");

        // Deletion WAL table
        conn.execute(
            "CREATE TABLE IF NOT EXISTS deletion_queue (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                user_id TEXT NOT NULL,
                bucket TEXT NOT NULL,
                key TEXT NOT NULL,
                offset_size_list BLOB NOT NULL,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                processed BOOLEAN DEFAULT FALSE
            )",
            [],
        ).expect("Failed to create deletion_queue table");

        // Bucket registry
        conn.execute(
            "CREATE TABLE IF NOT EXISTS buckets (
                user TEXT NOT NULL,
                name TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%S.000Z', 'now')),
                PRIMARY KEY (user, name)
            )",
            [],
        ).expect("Failed to create buckets table");

        // Incremental migrations — all use .ok() so they're idempotent
        conn.execute("ALTER TABLE haystack ADD COLUMN bucket TEXT DEFAULT 'default'", []).ok();
        conn.execute("ALTER TABLE haystack ADD COLUMN etag TEXT", []).ok();
        conn.execute("ALTER TABLE haystack ADD COLUMN size INTEGER NOT NULL DEFAULT 0", []).ok();
        conn.execute("ALTER TABLE haystack ADD COLUMN content_type TEXT", []).ok();
        conn.execute("ALTER TABLE haystack ADD COLUMN last_modified TEXT", []).ok();
        conn.execute("ALTER TABLE haystack ADD COLUMN user_metadata TEXT", []).ok();

        Arc::new(Mutex::new(conn))
    };
}

pub struct SQLiteMetadataStore;

impl SQLiteMetadataStore {
    pub fn new() -> Self { Self }
}

impl MetadataStorage for SQLiteMetadataStore {
    fn put_metadata(&self, user_id: &str, bucket: &str, object_id: &str, metadata: &Metadata) -> Result<(), Error> {
        let offset_size_list = metadata.to_offset_size_list();
        let offset_size_bytes = serialize_offset_size(&offset_size_list)?;
        let user_metadata_json = serde_json::to_string(&metadata.user_metadata)
            .unwrap_or_else(|_| "{}".to_string());

        let conn = DB_CONN.lock().unwrap();
        let result = conn.execute(
            "INSERT INTO haystack
                (user, bucket, key, offset_size_list, etag, size, content_type, last_modified, user_metadata)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                user_id,
                bucket,
                object_id,
                offset_size_bytes,
                metadata.etag,
                metadata.size as i64,
                metadata.content_type,
                metadata.last_modified,
                user_metadata_json,
            ],
        );
        match result {
            Ok(_) => Ok(()),
            Err(e) => {
                error!("put_metadata failed user={} bucket={} key={}: {}", user_id, bucket, object_id, e);
                Err(actix_web::error::ErrorInternalServerError(e))
            }
        }
    }

    fn get_metadata(&self, user_id: &str, bucket: &str, object_id: &str) -> Result<Metadata, Error> {
        let conn = DB_CONN.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT offset_size_list, etag, size, content_type, last_modified, user_metadata
             FROM haystack WHERE user = ?1 AND bucket = ?2 AND key = ?3",
        ).map_err(actix_web::error::ErrorInternalServerError)?;

        let row = stmt.query_row(params![user_id, bucket, object_id], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
            ))
        }).map_err(|e| {
            warn!("get_metadata: key not found user={} bucket={} key={}: {}", user_id, bucket, object_id, e);
            actix_web::error::ErrorNotFound(format!(
                "No data found for key: {} in bucket: {}, The key does not exist", object_id, bucket
            ))
        })?;

        let (offset_size_bytes, etag, size, content_type, last_modified, user_metadata_json) = row;
        let offset_size_list = crate::util::serializer::deserialize_offset_size(&offset_size_bytes)?;
        let user_metadata: std::collections::HashMap<String, String> = user_metadata_json
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default();

        let mut metadata = Metadata::from_offset_size_list(offset_size_list);
        metadata.etag = etag;
        metadata.size = size as u64;
        metadata.content_type = content_type;
        metadata.last_modified = last_modified;
        metadata.user_metadata = user_metadata;
        Ok(metadata)
    }

    fn delete_metadata(&self, user_id: &str, bucket: &str, object_id: &str) -> Result<(), Error> {
        let conn = DB_CONN.lock().unwrap();
        conn.execute(
            "DELETE FROM haystack WHERE user = ?1 AND bucket = ?2 AND key = ?3",
            params![user_id, bucket, object_id],
        ).map_err(actix_web::error::ErrorInternalServerError)?;
        Ok(())
    }

    fn list_objects(&self, user_id: &str, bucket: &str) -> Result<Vec<ObjectId>, Error> {
        let conn = DB_CONN.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT key FROM haystack WHERE user = ?1 AND bucket = ?2 ORDER BY key",
        ).map_err(actix_web::error::ErrorInternalServerError)?;

        let rows = stmt.query_map(params![user_id, bucket], |row| {
            row.get::<_, String>(0)
        }).map_err(actix_web::error::ErrorInternalServerError)?;

        let mut objects = Vec::new();
        for row in rows {
            objects.push(row.map_err(actix_web::error::ErrorInternalServerError)?);
        }
        Ok(objects)
    }

    fn object_exists(&self, user_id: &str, bucket: &str, object_id: &str) -> Result<bool, Error> {
        let conn = DB_CONN.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT COUNT(*) FROM haystack WHERE user = ?1 AND bucket = ?2 AND key = ?3",
        ).map_err(actix_web::error::ErrorInternalServerError)?;
        let count: i64 = stmt.query_row(params![user_id, bucket, object_id], |row| row.get(0))
            .map_err(actix_web::error::ErrorInternalServerError)?;
        Ok(count > 0)
    }

    fn update_metadata(&self, user_id: &str, bucket: &str, object_id: &str, metadata: &Metadata) -> Result<(), Error> {
        let offset_size_list = metadata.to_offset_size_list();
        let offset_size_bytes = serialize_offset_size(&offset_size_list)?;
        let user_metadata_json = serde_json::to_string(&metadata.user_metadata)
            .unwrap_or_else(|_| "{}".to_string());

        let conn = DB_CONN.lock().unwrap();
        conn.execute(
            "UPDATE haystack SET
                offset_size_list = ?1,
                etag = ?2,
                size = ?3,
                content_type = ?4,
                last_modified = ?5,
                user_metadata = ?6
             WHERE user = ?7 AND bucket = ?8 AND key = ?9",
            params![
                offset_size_bytes,
                metadata.etag,
                metadata.size as i64,
                metadata.content_type,
                metadata.last_modified,
                user_metadata_json,
                user_id,
                bucket,
                object_id,
            ],
        ).map_err(actix_web::error::ErrorInternalServerError)?;
        Ok(())
    }

    fn update_object_id(&self, user_id: &str, bucket: &str, old_object_id: &str, new_object_id: &str) -> Result<(), Error> {
        let conn = DB_CONN.lock().unwrap();
        conn.execute(
            "UPDATE haystack SET key = ?1 WHERE user = ?2 AND bucket = ?3 AND key = ?4",
            params![new_object_id, user_id, bucket, old_object_id],
        ).map_err(actix_web::error::ErrorInternalServerError)?;
        Ok(())
    }

    fn queue_deletion(&self, user_id: &str, bucket: &str, key: &str, offset_size_list: &[(u64, u64)]) -> Result<(), Error> {
        SQLiteMetadataStore::queue_deletion(self, user_id, bucket, key, offset_size_list)
    }

    fn list_buckets_with_stats(&self, user_id: &str) -> Result<Vec<BucketStats>, Error> {
        let conn = DB_CONN.lock().unwrap();
        // LEFT JOIN ensures empty buckets (registered in `buckets` table but no objects) are included.
        let mut stmt = conn.prepare(
            "SELECT b.name,
                    COUNT(h.key)         AS object_count,
                    COALESCE(SUM(h.size), 0) AS total_size
             FROM buckets b
             LEFT JOIN haystack h ON h.user = b.user AND h.bucket = b.name
             WHERE b.user = ?1
             GROUP BY b.name
             ORDER BY b.name",
        ).map_err(actix_web::error::ErrorInternalServerError)?;

        let rows = stmt.query_map(params![user_id], |row| {
            Ok(BucketStats {
                name: row.get(0)?,
                object_count: row.get::<_, i64>(1)? as u64,
                total_size: row.get::<_, i64>(2)? as u64,
            })
        }).map_err(actix_web::error::ErrorInternalServerError)?;

        let mut stats = Vec::new();
        for row in rows {
            stats.push(row.map_err(actix_web::error::ErrorInternalServerError)?);
        }
        Ok(stats)
    }

    fn create_bucket(&self, user_id: &str, bucket: &str) -> Result<(), Error> {
        let conn = DB_CONN.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO buckets (user, name) VALUES (?1, ?2)",
            params![user_id, bucket],
        ).map_err(actix_web::error::ErrorInternalServerError)?;
        Ok(())
    }

    fn delete_bucket(&self, user_id: &str, bucket: &str) -> Result<(), Error> {
        let conn = DB_CONN.lock().unwrap();
        conn.execute(
            "DELETE FROM buckets WHERE user = ?1 AND name = ?2",
            params![user_id, bucket],
        ).map_err(actix_web::error::ErrorInternalServerError)?;
        Ok(())
    }

    fn bucket_exists(&self, user_id: &str, bucket: &str) -> Result<bool, Error> {
        let conn = DB_CONN.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT COUNT(*) FROM buckets WHERE user = ?1 AND name = ?2",
        ).map_err(actix_web::error::ErrorInternalServerError)?;
        let count: i64 = stmt.query_row(params![user_id, bucket], |row| row.get(0))
            .map_err(actix_web::error::ErrorInternalServerError)?;
        Ok(count > 0)
    }

    fn list_all_buckets_for_user(&self, user_id: &str) -> Result<Vec<String>, Error> {
        let conn = DB_CONN.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT name FROM buckets WHERE user = ?1 ORDER BY name",
        ).map_err(actix_web::error::ErrorInternalServerError)?;
        let rows = stmt.query_map(params![user_id], |row| row.get::<_, String>(0))
            .map_err(actix_web::error::ErrorInternalServerError)?;
        let mut names = Vec::new();
        for row in rows {
            names.push(row.map_err(actix_web::error::ErrorInternalServerError)?);
        }
        Ok(names)
    }
}

/// Deletion queue — WAL for background storage GC
impl SQLiteMetadataStore {
    pub fn queue_deletion(&self, user_id: &str, bucket: &str, key: &str, offset_size_list: &[(u64, u64)]) -> Result<(), Error> {
        let offset_size_bytes = serialize_offset_size(&offset_size_list.to_vec())?;
        let conn = DB_CONN.lock().unwrap();
        conn.execute(
            "INSERT INTO deletion_queue (user_id, bucket, key, offset_size_list) VALUES (?1, ?2, ?3, ?4)",
            params![user_id, bucket, key, offset_size_bytes],
        ).map_err(actix_web::error::ErrorInternalServerError)?;
        info!("Queued deletion user={} bucket={} key={} chunks={}", user_id, bucket, key, offset_size_list.len());
        Ok(())
    }

    pub fn get_pending_deletions(&self, limit: i32) -> Result<Vec<DeletionEvent>, Error> {
        let conn = DB_CONN.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, user_id, bucket, key, offset_size_list, created_at
             FROM deletion_queue
             WHERE processed = FALSE
             ORDER BY created_at ASC
             LIMIT ?1",
        ).map_err(actix_web::error::ErrorInternalServerError)?;

        let rows = stmt.query_map(params![limit], |row| {
            let offset_size_bytes: Vec<u8> = row.get(4)?;
            let offset_size_list = crate::util::serializer::deserialize_offset_size(&offset_size_bytes)
                .map_err(|_| rusqlite::Error::InvalidColumnType(4, "BLOB".to_string(), rusqlite::types::Type::Blob))?;
            Ok(DeletionEvent {
                id: row.get(0)?,
                user_id: row.get(1)?,
                bucket: row.get(2)?,
                key: row.get(3)?,
                offset_size_list,
                created_at: row.get(5)?,
            })
        }).map_err(actix_web::error::ErrorInternalServerError)?;

        let mut events = Vec::new();
        for row in rows {
            events.push(row.map_err(actix_web::error::ErrorInternalServerError)?);
        }
        Ok(events)
    }

    pub fn mark_deletion_processed(&self, id: i64) -> Result<(), Error> {
        let conn = DB_CONN.lock().unwrap();
        conn.execute(
            "UPDATE deletion_queue SET processed = TRUE WHERE id = ?1",
            params![id],
        ).map_err(actix_web::error::ErrorInternalServerError)?;
        Ok(())
    }

    pub fn cleanup_old_deletions(&self) -> Result<usize, Error> {
        let conn = DB_CONN.lock().unwrap();
        let count = conn.execute(
            "DELETE FROM deletion_queue WHERE processed = TRUE AND created_at < datetime('now', '-7 days')",
            [],
        ).map_err(actix_web::error::ErrorInternalServerError)?;
        info!("Cleaned up {} old deletion events", count);
        Ok(count)
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sqlite_metadata_store_basic_operations() {
        let store = SQLiteMetadataStore::new();
        let user_id = "test_user_sqlite";
        let object_id = "test_object_sqlite";

        let mut metadata = Metadata::from_offset_size_list(vec![(100, 200), (300, 400)]);
        metadata.etag = Some("\"abc123\"".to_string());
        metadata.content_type = Some("text/plain".to_string());

        store.put_metadata(user_id, "default", object_id, &metadata).unwrap();

        assert!(store.object_exists(user_id, "default", object_id).unwrap());
        assert!(!store.object_exists(user_id, "default", "nonexistent").unwrap());

        let retrieved = store.get_metadata(user_id, "default", object_id).unwrap();
        assert_eq!(retrieved.chunks.len(), 2);
        assert_eq!(retrieved.to_offset_size_list(), vec![(100, 200), (300, 400)]);
        assert_eq!(retrieved.etag, Some("\"abc123\"".to_string()));

        let objects = store.list_objects(user_id, "default").unwrap();
        assert!(objects.contains(&object_id.to_string()));

        let new_metadata = Metadata::from_offset_size_list(vec![(500, 600)]);
        store.update_metadata(user_id, "default", object_id, &new_metadata).unwrap();
        let updated = store.get_metadata(user_id, "default", object_id).unwrap();
        assert_eq!(updated.to_offset_size_list(), vec![(500, 600)]);

        let new_object_id = "new_test_object_sqlite";
        store.update_object_id(user_id, "default", object_id, new_object_id).unwrap();
        assert!(!store.object_exists(user_id, "default", object_id).unwrap());
        assert!(store.object_exists(user_id, "default", new_object_id).unwrap());

        store.delete_metadata(user_id, "default", new_object_id).unwrap();
        assert!(!store.object_exists(user_id, "default", new_object_id).unwrap());
    }

    #[test]
    fn test_bucket_lifecycle() {
        let store = SQLiteMetadataStore::new();
        let user_id = "test_bucket_user";
        let bucket = "test-lifecycle-bucket";

        assert!(!store.bucket_exists(user_id, bucket).unwrap());
        store.create_bucket(user_id, bucket).unwrap();
        assert!(store.bucket_exists(user_id, bucket).unwrap());

        // Idempotent
        store.create_bucket(user_id, bucket).unwrap();

        let names = store.list_all_buckets_for_user(user_id).unwrap();
        assert!(names.contains(&bucket.to_string()));

        store.delete_bucket(user_id, bucket).unwrap();
        assert!(!store.bucket_exists(user_id, bucket).unwrap());
    }
}
