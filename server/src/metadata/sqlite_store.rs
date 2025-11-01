//! SQLite implementation of MetadataStorage trait

use crate::metadata::{MetadataStorage, Metadata, ObjectId};
use crate::util::serializer::{serialize_offset_size, deserialize_offset_size};
use std::sync::Mutex;
use rusqlite::{params, Connection};
use std::sync::Arc;
use log::{warn, info, error};
use actix_web::Error;
use lazy_static::lazy_static;
use std::env;
use std::path::{Path, PathBuf};

fn get_db_path() -> PathBuf {
    // Try to get the path from environment variable
    match env::var("DB_FILE") {
        Ok(path) => {
            info!("Using database path from environment: {}", path);
            PathBuf::from(path)
        }
        Err(_) => {
            warn!("Metadata database location not defined in environment");
            // Create default path: ./metadata/metadata.sqlite
            let default_path = Path::new("metadata").join("metadata.sqlite");
            
            // Create directory if it doesn't exist
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
        )
        .expect("Failed to create table");
        
        // Create deletion queue table for WAL
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
        )
        .expect("Failed to create deletion_queue table");
        
        // Add migration for existing data
        conn.execute(
            "ALTER TABLE haystack ADD COLUMN bucket TEXT DEFAULT 'default'",
            [],
        ).ok(); // Ignore error if column already exists
        
        Arc::new(Mutex::new(conn))
    };
}

/// SQLite implementation of MetadataStorage
pub struct SQLiteMetadataStore;

impl SQLiteMetadataStore {
    /// Create a new SQLite metadata store
    pub fn new() -> Self {
        Self
    }
}

impl MetadataStorage for SQLiteMetadataStore {
    fn put_metadata(&self, user_id: &str, bucket: &str, object_id: &str, metadata: &Metadata) -> Result<(), Error> {
        info!("SQLite put_metadata called for user: {}, bucket: {}, object_id: {}", user_id, bucket, object_id);
        let offset_size_list = metadata.to_offset_size_list();
        let offset_size_bytes = serialize_offset_size(&offset_size_list)?;
        info!("Serialized metadata, size: {} bytes", offset_size_bytes.len());
        
        let conn = DB_CONN.lock().unwrap();
        info!("Acquired database connection lock");
        
        let result = conn.execute(
            "INSERT INTO haystack (user, bucket, key, offset_size_list) VALUES (?1, ?2, ?3, ?4)",
            params![user_id, bucket, object_id, offset_size_bytes],
        );
        
        match result {
            Ok(_) => {
                info!("Successfully inserted metadata for user: {}, bucket: {}, object_id: {}", user_id, bucket, object_id);
                Ok(())
            },
            Err(e) => {
                error!("Failed to insert metadata for user: {}, bucket: {}, object_id: {}: {}", user_id, bucket, object_id, e);
                Err(actix_web::error::ErrorInternalServerError(e))
            }
        }
    }
    
    fn get_metadata(&self, user_id: &str, bucket: &str, object_id: &str) -> Result<Metadata, Error> {
        let conn = DB_CONN.lock().unwrap();
        let mut stmt = conn.prepare("SELECT offset_size_list FROM haystack WHERE user = ?1 AND bucket = ?2 AND key = ?3")
            .map_err(actix_web::error::ErrorInternalServerError)?;
        
        let offset_size_bytes = stmt.query_row(params![user_id, bucket, object_id], |row| {
            let offset_size_list: Vec<u8> = row.get(0)?;
            Ok(offset_size_list)
        }).map_err(|e| {
            warn!("Key does not exist or database error: {}", e);
            actix_web::error::ErrorNotFound(format!("No data found for key: {} in bucket: {}, The key does not exist", object_id, bucket))
        })?;
        
        let offset_size_list = deserialize_offset_size(&offset_size_bytes)?;
        Ok(Metadata::from_offset_size_list(offset_size_list))
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
        let mut stmt = conn.prepare("SELECT key FROM haystack WHERE user = ?1 AND bucket = ?2")
            .map_err(actix_web::error::ErrorInternalServerError)?;
        
        let rows = stmt.query_map(params![user_id, bucket], |row| {
            let key: String = row.get(0)?;
            Ok(key)
        }).map_err(actix_web::error::ErrorInternalServerError)?;
        
        let mut objects = Vec::new();
        for row in rows {
            objects.push(row.map_err(actix_web::error::ErrorInternalServerError)?);
        }
        
        Ok(objects)
    }
    
    fn object_exists(&self, user_id: &str, bucket: &str, object_id: &str) -> Result<bool, Error> {
        info!("SQLite object_exists called for user: {}, bucket: {}, object_id: {}", user_id, bucket, object_id);
        let conn = DB_CONN.lock().unwrap();
        let mut stmt = conn.prepare("SELECT COUNT(*) FROM haystack WHERE user = ?1 AND bucket = ?2 AND key = ?3")
            .map_err(actix_web::error::ErrorInternalServerError)?;
        
        let count: i64 = stmt.query_row(params![user_id, bucket, object_id], |row| row.get(0))
            .map_err(actix_web::error::ErrorInternalServerError)?;
        
        let exists = count > 0;
        info!("SQLite object_exists result: {} for user: {}, bucket: {}, object_id: {}", exists, user_id, bucket, object_id);
        Ok(exists)
    }
    
    fn update_metadata(&self, user_id: &str, bucket: &str, object_id: &str, metadata: &Metadata) -> Result<(), Error> {
        let offset_size_list = metadata.to_offset_size_list();
        let offset_size_bytes = serialize_offset_size(&offset_size_list)?;
        
        let conn = DB_CONN.lock().unwrap();
        conn.execute(
            "UPDATE haystack SET offset_size_list = ?1 WHERE user = ?2 AND bucket = ?3 AND key = ?4",
            params![offset_size_bytes, user_id, bucket, object_id],
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
}

/// Deletion queue operations for WAL functionality
impl SQLiteMetadataStore {
    /// Add a deletion event to the queue
    pub fn queue_deletion(&self, user_id: &str, bucket: &str, key: &str, offset_size_list: &[(u64, u64)]) -> Result<(), Error> {
        let offset_size_bytes = serialize_offset_size(&offset_size_list.to_vec())?;
        
        let conn = DB_CONN.lock().unwrap();
        conn.execute(
            "INSERT INTO deletion_queue (user_id, bucket, key, offset_size_list) VALUES (?1, ?2, ?3, ?4)",
            params![user_id, bucket, key, offset_size_bytes],
        ).map_err(actix_web::error::ErrorInternalServerError)?;
        
        info!("Queued deletion for user {} bucket {} key {} with {} chunks", 
              user_id, bucket, key, offset_size_list.len());
        Ok(())
    }
    
    /// Get pending deletion events (for worker processing)
    pub fn get_pending_deletions(&self, limit: i32) -> Result<Vec<DeletionEvent>, Error> {
        let conn = DB_CONN.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, user_id, bucket, key, offset_size_list, created_at 
             FROM deletion_queue 
             WHERE processed = FALSE 
             ORDER BY created_at ASC 
             LIMIT ?1"
        ).map_err(actix_web::error::ErrorInternalServerError)?;
        
        let rows = stmt.query_map(params![limit], |row| {
            let offset_size_bytes: Vec<u8> = row.get(4)?;
            let offset_size_list = deserialize_offset_size(&offset_size_bytes)
                .map_err(|_e| rusqlite::Error::InvalidColumnType(4, "BLOB".to_string(), rusqlite::types::Type::Blob))?;
            
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
    
    /// Mark deletion event as processed
    pub fn mark_deletion_processed(&self, id: i64) -> Result<(), Error> {
        let conn = DB_CONN.lock().unwrap();
        conn.execute(
            "UPDATE deletion_queue SET processed = TRUE WHERE id = ?1",
            params![id],
        ).map_err(actix_web::error::ErrorInternalServerError)?;
        
        Ok(())
    }
    
    /// Clean up old processed deletion events (older than 7 days)
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

/// Deletion event structure
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
        
        // Create test metadata
        let mut metadata = Metadata::from_offset_size_list(vec![(100, 200), (300, 400)]);
        metadata.properties.insert("test_prop".to_string(), "test_value".to_string());
        
        let user_id = "test_user_sqlite";
        let object_id = "test_object_sqlite";
        
        // Test put_metadata
        store.put_metadata(user_id, "default", object_id, &metadata).unwrap();
        
        // Test object_exists
        assert!(store.object_exists(user_id, "default", object_id).unwrap());
        assert!(!store.object_exists(user_id, "default", "nonexistent").unwrap());
        
        // Test get_metadata
        let retrieved = store.get_metadata(user_id, "default", object_id).unwrap();
        assert_eq!(retrieved.chunks.len(), 2);
        assert_eq!(retrieved.to_offset_size_list(), vec![(100, 200), (300, 400)]);
        
        // Test list_objects
        let objects = store.list_objects(user_id, "default").unwrap();
        assert!(objects.contains(&object_id.to_string()));
        
        // Test update_metadata
        let new_metadata = Metadata::from_offset_size_list(vec![(500, 600)]);
        store.update_metadata(user_id, "default", object_id, &new_metadata).unwrap();
        
        let updated = store.get_metadata(user_id, "default", object_id).unwrap();
        assert_eq!(updated.to_offset_size_list(), vec![(500, 600)]);
        
        // Test update_object_id
        let new_object_id = "new_test_object_sqlite";
        store.update_object_id(user_id, "default", object_id, new_object_id).unwrap();
        
        assert!(!store.object_exists(user_id, "default", object_id).unwrap());
        assert!(store.object_exists(user_id, "default", new_object_id).unwrap());
        
        // Test delete_metadata
        store.delete_metadata(user_id, "default", new_object_id).unwrap();
        assert!(!store.object_exists(user_id, "default", new_object_id).unwrap());
    }

}