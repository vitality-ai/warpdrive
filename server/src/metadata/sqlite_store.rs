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

        conn.execute_batch("PRAGMA journal_mode=WAL;").ok();

        // Object metadata table — stores S3 object extents + S3 metadata fields
        conn.execute(
            "CREATE TABLE IF NOT EXISTS objects (
                id               INTEGER PRIMARY KEY AUTOINCREMENT,
                user             TEXT NOT NULL,
                bucket           TEXT NOT NULL,
                key              TEXT NOT NULL,
                offset_size_list BLOB,
                etag             TEXT,
                size             INTEGER NOT NULL DEFAULT 0,
                content_type     TEXT,
                last_modified    TEXT,
                user_metadata    TEXT,
                cache_control    TEXT,
                expires          TEXT,
                content_encoding TEXT,
                UNIQUE(user, bucket, key)
            )",
            [],
        ).expect("Failed to create objects table");

        // Migrate existing databases that predate these columns
        conn.execute("ALTER TABLE objects ADD COLUMN cache_control TEXT", []).ok();
        conn.execute("ALTER TABLE objects ADD COLUMN expires TEXT", []).ok();
        conn.execute("ALTER TABLE objects ADD COLUMN content_encoding TEXT", []).ok();
        conn.execute("ALTER TABLE objects ADD COLUMN parts_manifest TEXT", []).ok();

        // Multipart upload tracking tables
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS multipart_uploads (
                upload_id     TEXT NOT NULL PRIMARY KEY,
                user_id       TEXT NOT NULL,
                bucket        TEXT NOT NULL,
                key           TEXT NOT NULL,
                content_type  TEXT,
                metadata_json TEXT NOT NULL DEFAULT '{}',
                initiated_at  TEXT NOT NULL,
                status        TEXT NOT NULL DEFAULT 'in_progress',
                final_etag    TEXT
            );
            CREATE TABLE IF NOT EXISTS multipart_parts (
                upload_id    TEXT NOT NULL,
                part_number  INTEGER NOT NULL,
                etag         TEXT NOT NULL,
                size         INTEGER NOT NULL,
                extents_blob BLOB NOT NULL,
                PRIMARY KEY (upload_id, part_number)
            );"
        ).expect("Failed to create multipart tables");

        // Deletion WAL — extent ranges queued for background GC
        conn.execute(
            "CREATE TABLE IF NOT EXISTS deletion_queue (
                id               INTEGER PRIMARY KEY AUTOINCREMENT,
                user_id          TEXT NOT NULL,
                bucket           TEXT NOT NULL,
                key              TEXT NOT NULL,
                offset_size_list BLOB NOT NULL,
                created_at       DATETIME DEFAULT CURRENT_TIMESTAMP,
                processed        BOOLEAN DEFAULT FALSE
            )",
            [],
        ).expect("Failed to create deletion_queue table");

        // Bucket registry — tracks created buckets (including empty ones)
        conn.execute(
            "CREATE TABLE IF NOT EXISTS buckets (
                user       TEXT NOT NULL,
                name       TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%S.000Z', 'now')),
                PRIMARY KEY (user, name)
            )",
            [],
        ).expect("Failed to create buckets table");

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
            "INSERT INTO objects
                (user, bucket, key, offset_size_list, etag, size, content_type, last_modified, user_metadata, cache_control, expires, content_encoding)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                user_id, bucket, object_id,
                offset_size_bytes,
                metadata.etag,
                metadata.size as i64,
                metadata.content_type,
                metadata.last_modified,
                user_metadata_json,
                metadata.cache_control,
                metadata.expires,
                metadata.content_encoding,
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
            "SELECT offset_size_list, etag, size, content_type, last_modified, user_metadata, cache_control, expires, content_encoding
             FROM objects WHERE user = ?1 AND bucket = ?2 AND key = ?3",
        ).map_err(actix_web::error::ErrorInternalServerError)?;

        let row = stmt.query_row(params![user_id, bucket, object_id], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, Option<String>>(8)?,
            ))
        }).map_err(|e| {
            warn!("get_metadata: not found user={} bucket={} key={}: {}", user_id, bucket, object_id, e);
            actix_web::error::ErrorNotFound(format!(
                "No data found for key: {} in bucket: {}, The key does not exist", object_id, bucket
            ))
        })?;

        let (offset_size_bytes, etag, size, content_type, last_modified, user_metadata_json, cache_control, expires, content_encoding) = row;
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
        metadata.cache_control = cache_control;
        metadata.expires = expires;
        metadata.content_encoding = content_encoding;
        Ok(metadata)
    }

    fn delete_metadata(&self, user_id: &str, bucket: &str, object_id: &str) -> Result<(), Error> {
        let conn = DB_CONN.lock().unwrap();
        conn.execute(
            "DELETE FROM objects WHERE user = ?1 AND bucket = ?2 AND key = ?3",
            params![user_id, bucket, object_id],
        ).map_err(actix_web::error::ErrorInternalServerError)?;
        Ok(())
    }

    fn list_objects(&self, user_id: &str, bucket: &str) -> Result<Vec<ObjectId>, Error> {
        let conn = DB_CONN.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT key FROM objects WHERE user = ?1 AND bucket = ?2 ORDER BY key",
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
            "SELECT COUNT(*) FROM objects WHERE user = ?1 AND bucket = ?2 AND key = ?3",
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
            "UPDATE objects SET
                offset_size_list = ?1,
                etag             = ?2,
                size             = ?3,
                content_type     = ?4,
                last_modified    = ?5,
                user_metadata    = ?6,
                cache_control    = ?7,
                expires          = ?8,
                content_encoding = ?9
             WHERE user = ?10 AND bucket = ?11 AND key = ?12",
            params![
                offset_size_bytes,
                metadata.etag, metadata.size as i64,
                metadata.content_type, metadata.last_modified,
                user_metadata_json,
                metadata.cache_control, metadata.expires,
                metadata.content_encoding,
                user_id, bucket, object_id,
            ],
        ).map_err(actix_web::error::ErrorInternalServerError)?;
        Ok(())
    }

    fn update_object_id(&self, user_id: &str, bucket: &str, old_object_id: &str, new_object_id: &str) -> Result<(), Error> {
        let conn = DB_CONN.lock().unwrap();
        conn.execute(
            "UPDATE objects SET key = ?1 WHERE user = ?2 AND bucket = ?3 AND key = ?4",
            params![new_object_id, user_id, bucket, old_object_id],
        ).map_err(actix_web::error::ErrorInternalServerError)?;
        Ok(())
    }

    fn queue_deletion(&self, user_id: &str, bucket: &str, key: &str, offset_size_list: &[(u64, u64)]) -> Result<(), Error> {
        SQLiteMetadataStore::queue_deletion(self, user_id, bucket, key, offset_size_list)
    }

    fn list_buckets_with_stats(&self, user_id: &str) -> Result<Vec<BucketStats>, Error> {
        let conn = DB_CONN.lock().unwrap();
        // LEFT JOIN so empty buckets still appear in the result
        let mut stmt = conn.prepare(
            "SELECT b.name,
                    b.created_at,
                    COUNT(o.key)             AS object_count,
                    COALESCE(SUM(o.size), 0) AS total_size
             FROM   buckets b
             LEFT JOIN objects o ON o.user = b.user AND o.bucket = b.name
             WHERE  b.user = ?1
             GROUP BY b.name
             ORDER BY b.name",
        ).map_err(actix_web::error::ErrorInternalServerError)?;

        let rows = stmt.query_map(params![user_id], |row| {
            Ok(BucketStats {
                name:         row.get(0)?,
                created_at:   row.get(1)?,
                object_count: row.get::<_, i64>(2)? as u64,
                total_size:   row.get::<_, i64>(3)? as u64,
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

    fn bucket_object_stats(&self, user_id: &str, bucket: &str) -> Result<(u64, u64), Error> {
        let conn = DB_CONN.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT COUNT(*), COALESCE(SUM(size), 0) FROM objects WHERE user = ?1 AND bucket = ?2",
        ).map_err(actix_web::error::ErrorInternalServerError)?;
        let (count, bytes): (i64, i64) = stmt.query_row(params![user_id, bucket], |row| {
            Ok((row.get(0)?, row.get(1)?))
        }).map_err(actix_web::error::ErrorInternalServerError)?;
        Ok((count as u64, bytes as u64))
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
}

#[derive(Debug, Clone)]
pub struct MultipartPartRow {
    pub part_number: i32,
    pub etag: String,
    pub size: u64,
    pub extents_blob: Vec<u8>,
}

/// Multipart upload management
impl SQLiteMetadataStore {
    pub fn create_multipart_upload(
        &self, upload_id: &str, user_id: &str, bucket: &str, key: &str,
        content_type: Option<&str>, metadata_json: &str, initiated_at: &str,
    ) -> Result<(), Error> {
        let conn = DB_CONN.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO multipart_uploads
             (upload_id, user_id, bucket, key, content_type, metadata_json, initiated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![upload_id, user_id, bucket, key, content_type, metadata_json, initiated_at],
        ).map_err(actix_web::error::ErrorInternalServerError)?;
        Ok(())
    }

    pub fn get_multipart_upload(&self, upload_id: &str) -> Result<Option<MultipartUploadRow>, Error> {
        let conn = DB_CONN.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT upload_id, user_id, bucket, key, content_type, metadata_json,
                    initiated_at, status, final_etag
             FROM multipart_uploads WHERE upload_id = ?1",
        ).map_err(actix_web::error::ErrorInternalServerError)?;
        let result = stmt.query_row(params![upload_id], |row| {
            Ok(MultipartUploadRow {
                upload_id: row.get(0)?,
                user_id: row.get(1)?,
                bucket: row.get(2)?,
                key: row.get(3)?,
                content_type: row.get(4)?,
                metadata_json: row.get::<_, Option<String>>(5)?.unwrap_or_else(|| "{}".to_string()),
                initiated_at: row.get(6)?,
                status: row.get::<_, String>(7)?,
                final_etag: row.get(8)?,
            })
        });
        match result {
            Ok(row) => Ok(Some(row)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(actix_web::error::ErrorInternalServerError(e)),
        }
    }

    pub fn mark_multipart_completed(&self, upload_id: &str, final_etag: &str) -> Result<(), Error> {
        let conn = DB_CONN.lock().unwrap();
        conn.execute(
            "UPDATE multipart_uploads SET status = 'completed', final_etag = ?1 WHERE upload_id = ?2",
            params![final_etag, upload_id],
        ).map_err(actix_web::error::ErrorInternalServerError)?;
        Ok(())
    }

    pub fn delete_multipart_upload(&self, upload_id: &str) -> Result<(), Error> {
        let conn = DB_CONN.lock().unwrap();
        conn.execute(
            "DELETE FROM multipart_uploads WHERE upload_id = ?1",
            params![upload_id],
        ).map_err(actix_web::error::ErrorInternalServerError)?;
        Ok(())
    }

    pub fn list_bucket_multipart_uploads(&self, bucket: &str) -> Result<Vec<MultipartUploadRow>, Error> {
        let conn = DB_CONN.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT upload_id, user_id, bucket, key, content_type, metadata_json,
                    initiated_at, status, final_etag
             FROM multipart_uploads
             WHERE bucket = ?1 AND status = 'in_progress'
             ORDER BY key, initiated_at",
        ).map_err(actix_web::error::ErrorInternalServerError)?;
        let rows = stmt.query_map(params![bucket], |row| {
            Ok(MultipartUploadRow {
                upload_id: row.get(0)?,
                user_id: row.get(1)?,
                bucket: row.get(2)?,
                key: row.get(3)?,
                content_type: row.get(4)?,
                metadata_json: row.get::<_, Option<String>>(5)?.unwrap_or_else(|| "{}".to_string()),
                initiated_at: row.get(6)?,
                status: row.get::<_, String>(7)?,
                final_etag: row.get(8)?,
            })
        }).map_err(actix_web::error::ErrorInternalServerError)?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row.map_err(actix_web::error::ErrorInternalServerError)?);
        }
        Ok(result)
    }

    pub fn upsert_multipart_part(
        &self, upload_id: &str, part_number: i32, etag: &str, size: u64, extents_blob: &[u8],
    ) -> Result<(), Error> {
        let conn = DB_CONN.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO multipart_parts (upload_id, part_number, etag, size, extents_blob)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![upload_id, part_number, etag, size as i64, extents_blob],
        ).map_err(actix_web::error::ErrorInternalServerError)?;
        Ok(())
    }

    pub fn list_multipart_parts(&self, upload_id: &str) -> Result<Vec<MultipartPartRow>, Error> {
        let conn = DB_CONN.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT part_number, etag, size, extents_blob
             FROM multipart_parts WHERE upload_id = ?1 ORDER BY part_number",
        ).map_err(actix_web::error::ErrorInternalServerError)?;
        let rows = stmt.query_map(params![upload_id], |row| {
            Ok(MultipartPartRow {
                part_number: row.get(0)?,
                etag: row.get(1)?,
                size: row.get::<_, i64>(2)? as u64,
                extents_blob: row.get(3)?,
            })
        }).map_err(actix_web::error::ErrorInternalServerError)?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row.map_err(actix_web::error::ErrorInternalServerError)?);
        }
        Ok(result)
    }

    pub fn delete_parts_for_upload(&self, upload_id: &str) -> Result<(), Error> {
        let conn = DB_CONN.lock().unwrap();
        conn.execute(
            "DELETE FROM multipart_parts WHERE upload_id = ?1",
            params![upload_id],
        ).map_err(actix_web::error::ErrorInternalServerError)?;
        Ok(())
    }

    pub fn get_parts_manifest(&self, user_id: &str, bucket: &str, key: &str) -> Result<Option<String>, Error> {
        let conn = DB_CONN.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT parts_manifest FROM objects WHERE user = ?1 AND bucket = ?2 AND key = ?3",
        ).map_err(actix_web::error::ErrorInternalServerError)?;
        let result = stmt.query_row(params![user_id, bucket, key], |row| {
            row.get::<_, Option<String>>(0)
        });
        match result {
            Ok(v) => Ok(v),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(actix_web::error::ErrorInternalServerError(e)),
        }
    }

    pub fn set_parts_manifest(&self, user_id: &str, bucket: &str, key: &str, manifest: &str) -> Result<(), Error> {
        let conn = DB_CONN.lock().unwrap();
        conn.execute(
            "UPDATE objects SET parts_manifest = ?1 WHERE user = ?2 AND bucket = ?3 AND key = ?4",
            params![manifest, user_id, bucket, key],
        ).map_err(actix_web::error::ErrorInternalServerError)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sqlite_objects_table_basic_operations() {
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
        assert_eq!(retrieved.to_offset_size_list(), vec![(100, 200), (300, 400)]);
        assert_eq!(retrieved.etag, Some("\"abc123\"".to_string()));

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
        store.create_bucket(user_id, bucket).unwrap(); // idempotent

        let names = store.list_all_buckets_for_user(user_id).unwrap();
        assert!(names.contains(&bucket.to_string()));

        store.delete_bucket(user_id, bucket).unwrap();
        assert!(!store.bucket_exists(user_id, bucket).unwrap());
    }
}
