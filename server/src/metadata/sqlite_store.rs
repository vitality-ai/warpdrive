//! SQLite implementation of MetadataStorage trait

use crate::metadata::{MetadataStorage, Metadata, ObjectId};
use crate::util::serializer::{serialize_offset_size, deserialize_offset_size};
use std::sync::Mutex;
use rusqlite::{params, Connection};
use std::sync::Arc;
use log::{warn, info};
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
                key TEXT NOT NULL,
                offset_size_list BLOB,
                UNIQUE(user, key)
            )",
            [],
        )
        .expect("Failed to create table");
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
    fn put_metadata(&self, user_id: &str, object_id: &str, metadata: &Metadata) -> Result<(), Error> {
        let offset_size_list = metadata.to_offset_size_list();
        let offset_size_bytes = serialize_offset_size(&offset_size_list)?;
        
        let conn = DB_CONN.lock().unwrap();
        conn.execute(
            "INSERT INTO haystack (user, key, offset_size_list) VALUES (?1, ?2, ?3)",
            params![user_id, object_id, offset_size_bytes],
        ).map_err(actix_web::error::ErrorInternalServerError)?;
        
        Ok(())
    }
    
    fn get_metadata(&self, user_id: &str, object_id: &str) -> Result<Metadata, Error> {
        let conn = DB_CONN.lock().unwrap();
        let mut stmt = conn.prepare("SELECT offset_size_list FROM haystack WHERE user = ?1 AND key = ?2")
            .map_err(actix_web::error::ErrorInternalServerError)?;
        
        let offset_size_bytes = stmt.query_row(params![user_id, object_id], |row| {
            let offset_size_list: Vec<u8> = row.get(0)?;
            Ok(offset_size_list)
        }).map_err(|e| {
            warn!("Key does not exist or database error: {}", e);
            actix_web::error::ErrorNotFound(format!("No data found for key: {}, The key does not exist", object_id))
        })?;
        
        let offset_size_list = deserialize_offset_size(&offset_size_bytes)?;
        Ok(Metadata::from_offset_size_list(offset_size_list))
    }
    
    fn delete_metadata(&self, user_id: &str, object_id: &str) -> Result<(), Error> {
        let conn = DB_CONN.lock().unwrap();
        conn.execute(
            "DELETE FROM haystack WHERE user = ?1 AND key = ?2",
            params![user_id, object_id],
        ).map_err(actix_web::error::ErrorInternalServerError)?;
        
        Ok(())
    }
    
    fn list_objects(&self, user_id: &str) -> Result<Vec<ObjectId>, Error> {
        let conn = DB_CONN.lock().unwrap();
        let mut stmt = conn.prepare("SELECT key FROM haystack WHERE user = ?1")
            .map_err(actix_web::error::ErrorInternalServerError)?;
        
        let rows = stmt.query_map(params![user_id], |row| {
            let key: String = row.get(0)?;
            Ok(key)
        }).map_err(actix_web::error::ErrorInternalServerError)?;
        
        let mut objects = Vec::new();
        for row in rows {
            objects.push(row.map_err(actix_web::error::ErrorInternalServerError)?);
        }
        
        Ok(objects)
    }
    
    fn object_exists(&self, user_id: &str, object_id: &str) -> Result<bool, Error> {
        let conn = DB_CONN.lock().unwrap();
        let mut stmt = conn.prepare("SELECT COUNT(*) FROM haystack WHERE user = ?1 AND key = ?2")
            .map_err(actix_web::error::ErrorInternalServerError)?;
        
        let count: i64 = stmt.query_row(params![user_id, object_id], |row| row.get(0))
            .map_err(actix_web::error::ErrorInternalServerError)?;
        
        Ok(count > 0)
    }
    
    fn update_metadata(&self, user_id: &str, object_id: &str, metadata: &Metadata) -> Result<(), Error> {
        let offset_size_list = metadata.to_offset_size_list();
        let offset_size_bytes = serialize_offset_size(&offset_size_list)?;
        
        let conn = DB_CONN.lock().unwrap();
        conn.execute(
            "UPDATE haystack SET offset_size_list = ?1 WHERE user = ?2 AND key = ?3",
            params![offset_size_bytes, user_id, object_id],
        ).map_err(actix_web::error::ErrorInternalServerError)?;
        
        Ok(())
    }
    
    fn update_object_id(&self, user_id: &str, old_object_id: &str, new_object_id: &str) -> Result<(), Error> {
        let conn = DB_CONN.lock().unwrap();
        conn.execute(
            "UPDATE haystack SET key = ?1 WHERE user = ?2 AND key = ?3",
            params![new_object_id, user_id, old_object_id],
        ).map_err(actix_web::error::ErrorInternalServerError)?;
        
        Ok(())
    }
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
        store.put_metadata(user_id, object_id, &metadata).unwrap();
        
        // Test object_exists
        assert!(store.object_exists(user_id, object_id).unwrap());
        assert!(!store.object_exists(user_id, "nonexistent").unwrap());
        
        // Test get_metadata
        let retrieved = store.get_metadata(user_id, object_id).unwrap();
        assert_eq!(retrieved.chunks.len(), 2);
        assert_eq!(retrieved.to_offset_size_list(), vec![(100, 200), (300, 400)]);
        
        // Test list_objects
        let objects = store.list_objects(user_id).unwrap();
        assert!(objects.contains(&object_id.to_string()));
        
        // Test update_metadata
        let new_metadata = Metadata::from_offset_size_list(vec![(500, 600)]);
        store.update_metadata(user_id, object_id, &new_metadata).unwrap();
        
        let updated = store.get_metadata(user_id, object_id).unwrap();
        assert_eq!(updated.to_offset_size_list(), vec![(500, 600)]);
        
        // Test update_object_id
        let new_object_id = "new_test_object_sqlite";
        store.update_object_id(user_id, object_id, new_object_id).unwrap();
        
        assert!(!store.object_exists(user_id, object_id).unwrap());
        assert!(store.object_exists(user_id, new_object_id).unwrap());
        
        // Test delete_metadata
        store.delete_metadata(user_id, new_object_id).unwrap();
        assert!(!store.object_exists(user_id, new_object_id).unwrap());
    }
}