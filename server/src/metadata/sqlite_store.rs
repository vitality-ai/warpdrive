//! SQLite implementation of MetadataStorage trait

use crate::metadata::{MetadataStorage, Metadata, ObjectId, BucketStats};
use crate::util::serializer::serialize_offset_size;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use rusqlite::{params, Connection};
use std::sync::Arc;
use log::{warn, info, error};
use actix_web::Error;
use lazy_static::lazy_static;
use std::env;
use std::path::{Path, PathBuf};

static VERSION_COUNTER: AtomicU64 = AtomicU64::new(0);

fn generate_version_id() -> String {
    let counter = VERSION_COUNTER.fetch_add(1, Ordering::SeqCst);
    let ts = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0) as u64;
    format!("{:016x}{:016x}", ts, counter)
}

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

        // Object metadata table — one row per (user, bucket, key, version_id).
        // version_id='' means versioning is disabled for that bucket.
        // version_id='null' means the null-version in a suspended-versioning bucket.
        // is_latest=1 marks the current visible version for a key.
        conn.execute(
            "CREATE TABLE IF NOT EXISTS objects (
                id               INTEGER PRIMARY KEY AUTOINCREMENT,
                user             TEXT NOT NULL,
                bucket           TEXT NOT NULL,
                key              TEXT NOT NULL,
                version_id       TEXT NOT NULL DEFAULT '',
                is_delete_marker INTEGER NOT NULL DEFAULT 0,
                is_latest        INTEGER NOT NULL DEFAULT 1,
                offset_size_list BLOB,
                etag             TEXT,
                size             INTEGER NOT NULL DEFAULT 0,
                content_type     TEXT,
                last_modified    TEXT,
                user_metadata    TEXT,
                cache_control    TEXT,
                expires          TEXT,
                content_encoding TEXT,
                parts_manifest   TEXT,
                UNIQUE(user, bucket, key, version_id)
            )",
            [],
        ).expect("Failed to create objects table");

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
                user              TEXT NOT NULL,
                name              TEXT NOT NULL,
                created_at        TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%S.000Z', 'now')),
                versioning_state  TEXT NOT NULL DEFAULT 'disabled',
                PRIMARY KEY (user, name)
            )",
            [],
        ).expect("Failed to create buckets table");
        conn.execute("ALTER TABLE buckets ADD COLUMN location TEXT DEFAULT ''", []).ok();

        // CORS configuration per bucket
        conn.execute(
            "CREATE TABLE IF NOT EXISTS bucket_cors (
                bucket  TEXT PRIMARY KEY,
                cors_xml TEXT NOT NULL
            )",
            [],
        ).expect("Failed to create bucket_cors table");

        // Object tags: replace-all semantics (DELETE + INSERT per PUT ?tagging)
        conn.execute(
            "CREATE TABLE IF NOT EXISTS object_tags (
                user_id   TEXT NOT NULL,
                bucket    TEXT NOT NULL,
                key       TEXT NOT NULL,
                tag_key   TEXT NOT NULL,
                tag_value TEXT NOT NULL DEFAULT '',
                PRIMARY KEY (user_id, bucket, key, tag_key)
            )",
            [],
        ).expect("Failed to create object_tags table");

        // Bucket tags
        conn.execute(
            "CREATE TABLE IF NOT EXISTS bucket_tags (
                bucket    TEXT NOT NULL,
                tag_key   TEXT NOT NULL,
                tag_value TEXT NOT NULL DEFAULT '',
                PRIMARY KEY (bucket, tag_key)
            )",
            [],
        ).expect("Failed to create bucket_tags table");

        // Tagging column on multipart_uploads (stores x-amz-tagging URL-encoded string)
        conn.execute("ALTER TABLE multipart_uploads ADD COLUMN tagging TEXT DEFAULT ''", []).ok();

        Arc::new(Mutex::new(conn))
    };
}

pub struct SQLiteMetadataStore;

impl SQLiteMetadataStore {
    pub fn new() -> Self { Self }
}

impl MetadataStorage for SQLiteMetadataStore {
    fn put_metadata(&self, user_id: &str, bucket: &str, object_id: &str, metadata: &Metadata) -> Result<(), Error> {
        // Non-versioned write: DELETE existing version_id='' row then INSERT new one.
        let offset_size_list = metadata.to_offset_size_list();
        let offset_size_bytes = serialize_offset_size(&offset_size_list)?;
        let user_metadata_json = serde_json::to_string(&metadata.user_metadata)
            .unwrap_or_else(|_| "{}".to_string());

        let conn = DB_CONN.lock().unwrap();
        conn.execute(
            "DELETE FROM objects WHERE user = ?1 AND bucket = ?2 AND key = ?3 AND version_id = ''",
            params![user_id, bucket, object_id],
        ).map_err(actix_web::error::ErrorInternalServerError)?;
        // Also clear is_latest on all existing versions so the non-versioned row becomes latest.
        conn.execute(
            "UPDATE objects SET is_latest = 0 WHERE user = ?1 AND bucket = ?2 AND key = ?3 AND is_latest = 1",
            params![user_id, bucket, object_id],
        ).map_err(actix_web::error::ErrorInternalServerError)?;
        let result = conn.execute(
            "INSERT INTO objects
                (user, bucket, key, version_id, is_latest, is_delete_marker,
                 offset_size_list, etag, size, content_type, last_modified,
                 user_metadata, cache_control, expires, content_encoding)
             VALUES (?1, ?2, ?3, '', 1, 0, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
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
            "SELECT offset_size_list, etag, size, content_type, last_modified, user_metadata,
                    cache_control, expires, content_encoding, version_id, is_delete_marker
             FROM objects
             WHERE user = ?1 AND bucket = ?2 AND key = ?3 AND is_latest = 1",
        ).map_err(actix_web::error::ErrorInternalServerError)?;

        let row = stmt.query_row(params![user_id, bucket, object_id], |row| {
            Ok((
                row.get::<_, Option<Vec<u8>>>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, String>(9)?,
                row.get::<_, i64>(10)?,
            ))
        }).map_err(|e| {
            warn!("get_metadata: not found user={} bucket={} key={}: {}", user_id, bucket, object_id, e);
            actix_web::error::ErrorNotFound(format!(
                "No data found for key: {} in bucket: {}, The key does not exist", object_id, bucket
            ))
        })?;

        let (offset_size_bytes, etag, size, content_type, last_modified, user_metadata_json,
             cache_control, expires, content_encoding, version_id, is_delete_marker) = row;

        if is_delete_marker != 0 {
            return Err(actix_web::error::ErrorNotFound(format!(
                "No data found for key: {} in bucket: {}, The key does not exist", object_id, bucket
            )));
        }

        let offset_size_list = if let Some(bytes) = offset_size_bytes {
            crate::util::serializer::deserialize_offset_size(&bytes)?
        } else {
            vec![]
        };
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
        metadata.version_id = if version_id.is_empty() { None } else { Some(version_id) };
        Ok(metadata)
    }

    fn delete_metadata(&self, user_id: &str, bucket: &str, object_id: &str) -> Result<(), Error> {
        // Hard-delete all rows for this key (used by CompleteMultipartUpload overwrite and internal cleanup).
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
            "SELECT key FROM objects
             WHERE user = ?1 AND bucket = ?2 AND is_latest = 1 AND is_delete_marker = 0
             ORDER BY key",
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
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM objects
             WHERE user = ?1 AND bucket = ?2 AND key = ?3 AND is_latest = 1 AND is_delete_marker = 0",
            params![user_id, bucket, object_id],
            |row| row.get(0),
        ).map_err(actix_web::error::ErrorInternalServerError)?;
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
             WHERE user = ?10 AND bucket = ?11 AND key = ?12 AND is_latest = 1",
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
                                 AND o.is_latest = 1 AND o.is_delete_marker = 0
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
            "SELECT COUNT(*), COALESCE(SUM(size), 0) FROM objects
             WHERE user = ?1 AND bucket = ?2 AND is_latest = 1 AND is_delete_marker = 0",
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

/// CORS and bucket location operations
impl SQLiteMetadataStore {
    pub fn set_bucket_cors(&self, bucket: &str, cors_xml: &str) -> Result<(), Error> {
        let conn = DB_CONN.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO bucket_cors (bucket, cors_xml) VALUES (?1, ?2)",
            params![bucket, cors_xml],
        ).map_err(actix_web::error::ErrorInternalServerError)?;
        Ok(())
    }

    pub fn get_bucket_cors(&self, bucket: &str) -> Result<Option<String>, Error> {
        let conn = DB_CONN.lock().unwrap();
        let mut stmt = conn.prepare("SELECT cors_xml FROM bucket_cors WHERE bucket = ?1")
            .map_err(actix_web::error::ErrorInternalServerError)?;
        let result = stmt.query_row(params![bucket], |row| row.get::<_, String>(0));
        match result {
            Ok(xml) => Ok(Some(xml)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(actix_web::error::ErrorInternalServerError(e)),
        }
    }

    pub fn delete_bucket_cors(&self, bucket: &str) -> Result<(), Error> {
        let conn = DB_CONN.lock().unwrap();
        conn.execute("DELETE FROM bucket_cors WHERE bucket = ?1", params![bucket])
            .map_err(actix_web::error::ErrorInternalServerError)?;
        Ok(())
    }

    pub fn set_bucket_location(&self, user_id: &str, bucket: &str, location: &str) -> Result<(), Error> {
        let conn = DB_CONN.lock().unwrap();
        conn.execute(
            "UPDATE buckets SET location = ?1 WHERE user = ?2 AND name = ?3",
            params![location, user_id, bucket],
        ).map_err(actix_web::error::ErrorInternalServerError)?;
        Ok(())
    }

    pub fn get_bucket_location(&self, user_id: &str, bucket: &str) -> Result<String, Error> {
        let conn = DB_CONN.lock().unwrap();
        let mut stmt = conn.prepare("SELECT location FROM buckets WHERE user = ?1 AND name = ?2")
            .map_err(actix_web::error::ErrorInternalServerError)?;
        let result = stmt.query_row(params![user_id, bucket], |row| row.get::<_, Option<String>>(0));
        match result {
            Ok(loc) => Ok(loc.unwrap_or_default()),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(String::new()),
            Err(e) => Err(actix_web::error::ErrorInternalServerError(e)),
        }
    }
}

/// Object + bucket tagging operations
impl SQLiteMetadataStore {
    /// Replace all tags for an object (atomic delete-then-insert within one lock).
    pub fn set_object_tags(&self, user_id: &str, bucket: &str, key: &str, tags: &[(String, String)]) -> Result<(), Error> {
        let conn = DB_CONN.lock().unwrap();
        conn.execute(
            "DELETE FROM object_tags WHERE user_id = ?1 AND bucket = ?2 AND key = ?3",
            params![user_id, bucket, key],
        ).map_err(actix_web::error::ErrorInternalServerError)?;
        for (k, v) in tags {
            conn.execute(
                "INSERT INTO object_tags (user_id, bucket, key, tag_key, tag_value) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![user_id, bucket, key, k, v],
            ).map_err(actix_web::error::ErrorInternalServerError)?;
        }
        Ok(())
    }

    pub fn get_object_tags(&self, user_id: &str, bucket: &str, key: &str) -> Result<Vec<(String, String)>, Error> {
        let conn = DB_CONN.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT tag_key, tag_value FROM object_tags WHERE user_id = ?1 AND bucket = ?2 AND key = ?3 ORDER BY tag_key",
        ).map_err(actix_web::error::ErrorInternalServerError)?;
        let rows = stmt.query_map(params![user_id, bucket, key], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        }).map_err(actix_web::error::ErrorInternalServerError)?;
        let mut tags = Vec::new();
        for row in rows {
            tags.push(row.map_err(actix_web::error::ErrorInternalServerError)?);
        }
        Ok(tags)
    }

    pub fn delete_object_tags(&self, user_id: &str, bucket: &str, key: &str) -> Result<(), Error> {
        let conn = DB_CONN.lock().unwrap();
        conn.execute(
            "DELETE FROM object_tags WHERE user_id = ?1 AND bucket = ?2 AND key = ?3",
            params![user_id, bucket, key],
        ).map_err(actix_web::error::ErrorInternalServerError)?;
        Ok(())
    }

    pub fn get_object_tag_count(&self, user_id: &str, bucket: &str, key: &str) -> Result<i64, Error> {
        let conn = DB_CONN.lock().unwrap();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM object_tags WHERE user_id = ?1 AND bucket = ?2 AND key = ?3",
            params![user_id, bucket, key],
            |row| row.get(0),
        ).map_err(actix_web::error::ErrorInternalServerError)?;
        Ok(count)
    }

    pub fn set_bucket_tags(&self, bucket: &str, tags: &[(String, String)]) -> Result<(), Error> {
        let conn = DB_CONN.lock().unwrap();
        conn.execute("DELETE FROM bucket_tags WHERE bucket = ?1", params![bucket])
            .map_err(actix_web::error::ErrorInternalServerError)?;
        for (k, v) in tags {
            conn.execute(
                "INSERT INTO bucket_tags (bucket, tag_key, tag_value) VALUES (?1, ?2, ?3)",
                params![bucket, k, v],
            ).map_err(actix_web::error::ErrorInternalServerError)?;
        }
        Ok(())
    }

    pub fn get_bucket_tags(&self, bucket: &str) -> Result<Vec<(String, String)>, Error> {
        let conn = DB_CONN.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT tag_key, tag_value FROM bucket_tags WHERE bucket = ?1 ORDER BY tag_key",
        ).map_err(actix_web::error::ErrorInternalServerError)?;
        let rows = stmt.query_map(params![bucket], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        }).map_err(actix_web::error::ErrorInternalServerError)?;
        let mut tags = Vec::new();
        for row in rows {
            tags.push(row.map_err(actix_web::error::ErrorInternalServerError)?);
        }
        Ok(tags)
    }

    pub fn delete_bucket_tags(&self, bucket: &str) -> Result<(), Error> {
        let conn = DB_CONN.lock().unwrap();
        conn.execute("DELETE FROM bucket_tags WHERE bucket = ?1", params![bucket])
            .map_err(actix_web::error::ErrorInternalServerError)?;
        Ok(())
    }

    pub fn set_multipart_tagging(&self, upload_id: &str, tagging: &str) -> Result<(), Error> {
        let conn = DB_CONN.lock().unwrap();
        conn.execute(
            "UPDATE multipart_uploads SET tagging = ?1 WHERE upload_id = ?2",
            params![tagging, upload_id],
        ).map_err(actix_web::error::ErrorInternalServerError)?;
        Ok(())
    }

    pub fn get_multipart_tagging(&self, upload_id: &str) -> Result<String, Error> {
        let conn = DB_CONN.lock().unwrap();
        let result: rusqlite::Result<Option<String>> = conn.query_row(
            "SELECT tagging FROM multipart_uploads WHERE upload_id = ?1",
            params![upload_id],
            |row| row.get(0),
        );
        match result {
            Ok(t) => Ok(t.unwrap_or_default()),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(String::new()),
            Err(e) => Err(actix_web::error::ErrorInternalServerError(e)),
        }
    }
}

/// Versioning operations
impl SQLiteMetadataStore {
    pub fn get_versioning_state(&self, bucket: &str) -> Result<String, Error> {
        let conn = DB_CONN.lock().unwrap();
        let result: rusqlite::Result<Option<String>> = conn.query_row(
            "SELECT versioning_state FROM buckets WHERE name = ?1",
            params![bucket],
            |row| row.get(0),
        );
        match result {
            Ok(s) => Ok(s.unwrap_or_else(|| "disabled".to_string())),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok("disabled".to_string()),
            Err(e) => Err(actix_web::error::ErrorInternalServerError(e)),
        }
    }

    pub fn set_versioning_state(&self, user_id: &str, bucket: &str, state: &str) -> Result<(), Error> {
        let conn = DB_CONN.lock().unwrap();
        conn.execute(
            "UPDATE buckets SET versioning_state = ?1 WHERE user = ?2 AND name = ?3",
            params![state, user_id, bucket],
        ).map_err(actix_web::error::ErrorInternalServerError)?;
        Ok(())
    }

    /// Versioning-aware PUT. Returns Some(version_id) when versioning is enabled/suspended,
    /// None when versioning is disabled (overwrites in place).
    /// Write a new object version.
    /// Returns (version_id, old_extents_to_gc).
    /// - version_id: Some(vid) when versioning enabled/suspended, None when disabled.
    /// - old_extents_to_gc: extents of the row that was replaced (caller should queue for GC).
    pub fn put_object_v2(
        &self, user_id: &str, bucket: &str, key: &str, metadata: &Metadata,
    ) -> Result<(Option<String>, Vec<(u64, u64)>), Error> {
        let versioning = self.get_versioning_state(bucket)?;
        let offset_size_list = metadata.to_offset_size_list();
        let offset_size_bytes = serialize_offset_size(&offset_size_list)?;
        let user_metadata_json = serde_json::to_string(&metadata.user_metadata)
            .unwrap_or_else(|_| "{}".to_string());

        let conn = DB_CONN.lock().unwrap();

        match versioning.as_str() {
            "disabled" => {
                // Read old extents before deleting so the caller can GC them.
                let old_extents: Vec<(u64, u64)> = conn.query_row(
                    "SELECT offset_size_list FROM objects WHERE user=?1 AND bucket=?2 AND key=?3 AND version_id=''",
                    params![user_id, bucket, key],
                    |row| row.get::<_, Option<Vec<u8>>>(0),
                ).unwrap_or(None)
                .and_then(|b| crate::util::serializer::deserialize_offset_size(&b).ok())
                .unwrap_or_default();

                conn.execute(
                    "DELETE FROM objects WHERE user=?1 AND bucket=?2 AND key=?3 AND version_id=''",
                    params![user_id, bucket, key],
                ).map_err(actix_web::error::ErrorInternalServerError)?;
                conn.execute(
                    "INSERT INTO objects
                        (user,bucket,key,version_id,is_latest,is_delete_marker,
                         offset_size_list,etag,size,content_type,last_modified,
                         user_metadata,cache_control,expires,content_encoding,parts_manifest)
                     VALUES(?1,?2,?3,'',1,0,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
                    params![user_id,bucket,key,offset_size_bytes,metadata.etag,
                            metadata.size as i64,metadata.content_type,metadata.last_modified,
                            user_metadata_json,metadata.cache_control,metadata.expires,
                            metadata.content_encoding,metadata.properties.get("parts_manifest")],
                ).map_err(actix_web::error::ErrorInternalServerError)?;
                Ok((None, old_extents))
            }
            "enabled" => {
                let vid = generate_version_id();
                // Demote current latest; old versions stay in DB (versioning preserves them).
                conn.execute(
                    "UPDATE objects SET is_latest=0 WHERE user=?1 AND bucket=?2 AND key=?3 AND is_latest=1",
                    params![user_id, bucket, key],
                ).map_err(actix_web::error::ErrorInternalServerError)?;
                conn.execute(
                    "INSERT INTO objects
                        (user,bucket,key,version_id,is_latest,is_delete_marker,
                         offset_size_list,etag,size,content_type,last_modified,
                         user_metadata,cache_control,expires,content_encoding,parts_manifest)
                     VALUES(?1,?2,?3,?4,1,0,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
                    params![user_id,bucket,key,vid,offset_size_bytes,metadata.etag,
                            metadata.size as i64,metadata.content_type,metadata.last_modified,
                            user_metadata_json,metadata.cache_control,metadata.expires,
                            metadata.content_encoding,metadata.properties.get("parts_manifest")],
                ).map_err(actix_web::error::ErrorInternalServerError)?;
                Ok((Some(vid), vec![]))
            }
            _ /* "suspended" */ => {
                // Read extents from whichever null-variant exists: 'null' (prior suspended write)
                // or '' (object written before versioning was ever enabled).
                let extents_from = |vid: &str| -> Vec<(u64, u64)> {
                    conn.query_row(
                        "SELECT offset_size_list FROM objects WHERE user=?1 AND bucket=?2 AND key=?3 AND version_id=?4",
                        params![user_id, bucket, key, vid],
                        |row| row.get::<_, Option<Vec<u8>>>(0),
                    ).unwrap_or(None)
                    .and_then(|b| crate::util::serializer::deserialize_offset_size(&b).ok())
                    .unwrap_or_default()
                };
                let old_extents: Vec<(u64, u64)> = {
                    let ne = extents_from("null");
                    if !ne.is_empty() { ne } else { extents_from("") }
                };

                // Delete both the null-version and the pre-versioning non-versioned row.
                conn.execute(
                    "DELETE FROM objects WHERE user=?1 AND bucket=?2 AND key=?3 AND version_id='null'",
                    params![user_id, bucket, key],
                ).map_err(actix_web::error::ErrorInternalServerError)?;
                conn.execute(
                    "DELETE FROM objects WHERE user=?1 AND bucket=?2 AND key=?3 AND version_id=''",
                    params![user_id, bucket, key],
                ).map_err(actix_web::error::ErrorInternalServerError)?;
                conn.execute(
                    "UPDATE objects SET is_latest=0 WHERE user=?1 AND bucket=?2 AND key=?3 AND is_latest=1",
                    params![user_id, bucket, key],
                ).map_err(actix_web::error::ErrorInternalServerError)?;
                conn.execute(
                    "INSERT INTO objects
                        (user,bucket,key,version_id,is_latest,is_delete_marker,
                         offset_size_list,etag,size,content_type,last_modified,
                         user_metadata,cache_control,expires,content_encoding,parts_manifest)
                     VALUES(?1,?2,?3,'null',1,0,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
                    params![user_id,bucket,key,offset_size_bytes,metadata.etag,
                            metadata.size as i64,metadata.content_type,metadata.last_modified,
                            user_metadata_json,metadata.cache_control,metadata.expires,
                            metadata.content_encoding,metadata.properties.get("parts_manifest")],
                ).map_err(actix_web::error::ErrorInternalServerError)?;
                Ok((Some("null".to_string()), old_extents))
            }
        }
    }

    /// Versioning-aware DELETE (no explicit versionId).
    pub fn delete_object_v2(&self, user_id: &str, bucket: &str, key: &str) -> Result<VersioningDeleteResult, Error> {
        let versioning = self.get_versioning_state(bucket)?;
        let conn = DB_CONN.lock().unwrap();

        match versioning.as_str() {
            "disabled" => {
                conn.execute(
                    "DELETE FROM objects WHERE user=?1 AND bucket=?2 AND key=?3",
                    params![user_id, bucket, key],
                ).map_err(actix_web::error::ErrorInternalServerError)?;
                Ok(VersioningDeleteResult::Deleted)
            }
            "enabled" => {
                let vid = generate_version_id();
                let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S.000Z").to_string();
                conn.execute(
                    "UPDATE objects SET is_latest=0 WHERE user=?1 AND bucket=?2 AND key=?3 AND is_latest=1",
                    params![user_id, bucket, key],
                ).map_err(actix_web::error::ErrorInternalServerError)?;
                conn.execute(
                    "INSERT INTO objects
                        (user,bucket,key,version_id,is_latest,is_delete_marker,size,last_modified)
                     VALUES(?1,?2,?3,?4,1,1,0,?5)",
                    params![user_id, bucket, key, vid, now],
                ).map_err(actix_web::error::ErrorInternalServerError)?;
                Ok(VersioningDeleteResult::Marker { version_id: vid })
            }
            _ /* "suspended" */ => {
                // Delete the existing null-version object (if it exists) and insert a null-version delete marker.
                conn.execute(
                    "DELETE FROM objects WHERE user=?1 AND bucket=?2 AND key=?3 AND version_id='null'",
                    params![user_id, bucket, key],
                ).map_err(actix_web::error::ErrorInternalServerError)?;
                conn.execute(
                    "UPDATE objects SET is_latest=0 WHERE user=?1 AND bucket=?2 AND key=?3 AND is_latest=1",
                    params![user_id, bucket, key],
                ).map_err(actix_web::error::ErrorInternalServerError)?;
                let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S.000Z").to_string();
                conn.execute(
                    "INSERT INTO objects
                        (user,bucket,key,version_id,is_latest,is_delete_marker,size,last_modified)
                     VALUES(?1,?2,?3,'null',1,1,0,?4)",
                    params![user_id, bucket, key, now],
                ).map_err(actix_web::error::ErrorInternalServerError)?;
                Ok(VersioningDeleteResult::Marker { version_id: "null".to_string() })
            }
        }
    }

    /// Permanently delete a specific version (DELETE ?versionId=x).
    /// Returns info about what was deleted.
    pub fn delete_specific_version(&self, user_id: &str, bucket: &str, key: &str, version_id: &str) -> Result<DeleteSpecificResult, Error> {
        let conn = DB_CONN.lock().unwrap();

        // "null" from the client matches both '' (versioning never enabled) and 'null' (suspended null-version).
        let effective_vid: &str = if version_id == "null" {
            let exists_empty: bool = conn.query_row(
                "SELECT 1 FROM objects WHERE user=?1 AND bucket=?2 AND key=?3 AND version_id=''",
                params![user_id, bucket, key], |_| Ok(true),
            ).unwrap_or(false);
            if exists_empty { "" } else { "null" }
        } else {
            version_id
        };

        // Fetch the row first to know is_delete_marker and is_latest.
        let row_result: rusqlite::Result<(i64, i64)> = conn.query_row(
            "SELECT is_delete_marker, is_latest FROM objects WHERE user=?1 AND bucket=?2 AND key=?3 AND version_id=?4",
            params![user_id, bucket, key, effective_vid],
            |row| Ok((row.get(0)?, row.get(1)?)),
        );

        match row_result {
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                return Ok(DeleteSpecificResult { found: false, was_delete_marker: false, version_id: version_id.to_string() });
            }
            Err(e) => return Err(actix_web::error::ErrorInternalServerError(e)),
            Ok(_) => {}
        }
        let (is_dm, was_latest) = row_result.unwrap();

        // Delete the row.
        conn.execute(
            "DELETE FROM objects WHERE user=?1 AND bucket=?2 AND key=?3 AND version_id=?4",
            params![user_id, bucket, key, effective_vid],
        ).map_err(actix_web::error::ErrorInternalServerError)?;

        // If the deleted row was is_latest, promote the most-recently-inserted remaining row.
        if was_latest != 0 {
            conn.execute(
                "UPDATE objects SET is_latest=1 WHERE id=(
                     SELECT id FROM objects WHERE user=?1 AND bucket=?2 AND key=?3 ORDER BY id DESC LIMIT 1
                 )",
                params![user_id, bucket, key],
            ).map_err(actix_web::error::ErrorInternalServerError)?;
        }

        Ok(DeleteSpecificResult {
            found: true,
            was_delete_marker: is_dm != 0,
            version_id: version_id.to_string(),
        })
    }

    /// Fetch a specific version of an object.
    pub fn get_object_version(&self, user_id: &str, bucket: &str, key: &str, version_id: &str) -> Result<Metadata, Error> {
        let conn = DB_CONN.lock().unwrap();
        // "null" matches '' (versioning never enabled) or 'null' (suspended null-version).
        let effective_vid: &str = if version_id == "null" {
            let exists_empty: bool = conn.query_row(
                "SELECT 1 FROM objects WHERE user=?1 AND bucket=?2 AND key=?3 AND version_id=''",
                params![user_id, bucket, key], |_| Ok(true),
            ).unwrap_or(false);
            if exists_empty { "" } else { "null" }
        } else {
            version_id
        };
        let row = conn.query_row(
            "SELECT offset_size_list,etag,size,content_type,last_modified,user_metadata,
                    cache_control,expires,content_encoding,version_id,is_delete_marker
             FROM objects WHERE user=?1 AND bucket=?2 AND key=?3 AND version_id=?4",
            params![user_id, bucket, key, effective_vid],
            |row| Ok((
                row.get::<_, Option<Vec<u8>>>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, String>(9)?,
                row.get::<_, i64>(10)?,
            )),
        ).map_err(|e| {
            if e == rusqlite::Error::QueryReturnedNoRows {
                actix_web::error::ErrorNotFound("Version not found")
            } else {
                actix_web::error::ErrorInternalServerError(e)
            }
        })?;

        let (offset_size_bytes, etag, size, content_type, last_modified, user_metadata_json,
             cache_control, expires, content_encoding, vid, is_delete_marker) = row;

        let offset_size_list = if let Some(bytes) = offset_size_bytes {
            crate::util::serializer::deserialize_offset_size(&bytes)?
        } else {
            vec![]
        };
        let user_metadata = user_metadata_json
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
        metadata.version_id = if vid.is_empty() { None } else { Some(vid) };
        metadata.is_delete_marker = is_delete_marker != 0;
        Ok(metadata)
    }

    /// List all versions for a bucket (for GET ?versions).
    /// Returns (rows, is_truncated, next_key_marker, next_version_id_marker).
    pub fn list_object_versions_full(
        &self, user_id: &str, bucket: &str,
        prefix: &str, key_marker: &str, version_id_marker: &str, max_keys: usize,
    ) -> Result<(Vec<VersionRow>, bool, String, String), Error> {
        let conn = DB_CONN.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT key, version_id, is_delete_marker, etag, size, last_modified, is_latest
             FROM objects
             WHERE user=?1 AND bucket=?2
             ORDER BY key ASC, id DESC",
        ).map_err(actix_web::error::ErrorInternalServerError)?;

        let rows = stmt.query_map(params![user_id, bucket], |row| {
            Ok(VersionRow {
                key:               row.get(0)?,
                version_id:        row.get(1)?,
                is_delete_marker:  row.get::<_, i64>(2)? != 0,
                etag:              row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                size:              row.get::<_, i64>(4)? as u64,
                last_modified:     row.get::<_, Option<String>>(5)?.unwrap_or_default(),
                is_latest:         row.get::<_, i64>(6)? != 0,
            })
        }).map_err(actix_web::error::ErrorInternalServerError)?;

        let mut all: Vec<VersionRow> = rows
            .filter_map(|r| r.ok())
            .filter(|r| r.key.starts_with(prefix))
            .collect();

        // Apply key-marker / version-id-marker pagination.
        if !key_marker.is_empty() {
            let mut past_marker = false;
            all.retain(|r| {
                if past_marker { return true; }
                if r.key.as_str() > key_marker { past_marker = true; return true; }
                if r.key.as_str() == key_marker && !version_id_marker.is_empty() && r.version_id.as_str() > version_id_marker {
                    return true;
                }
                false
            });
        }

        let is_truncated = all.len() > max_keys;
        let page: Vec<VersionRow> = all.into_iter().take(max_keys).collect();
        let (next_key, next_vid) = if is_truncated {
            page.last().map(|r| (r.key.clone(), r.version_id.clone())).unwrap_or_default()
        } else {
            (String::new(), String::new())
        };

        Ok((page, is_truncated, next_key, next_vid))
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

    pub fn delete_completed_uploads_for_key(&self, bucket: &str, key: &str) -> Result<(), Error> {
        let conn = DB_CONN.lock().unwrap();
        conn.execute(
            "DELETE FROM multipart_uploads WHERE bucket = ?1 AND key = ?2 AND status = 'completed'",
            params![bucket, key],
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
