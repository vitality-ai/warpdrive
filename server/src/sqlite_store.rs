//! sqlite_store.rs
//! 
//! SQLite implementation of the MetadataStorage trait.
//! This module wraps the existing SQLite database functionality.

use std::sync::Mutex;
use rusqlite::{params, Connection, Result as SqliteResult};
use std::sync::Arc;
use log::{warn, info};
use actix_web::Error;
use lazy_static::lazy_static;
use std::env;
use std::path::{Path, PathBuf};

use crate::metadata::{MetadataStorage, Metadata, ObjectId};

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

    /// Check if a key exists (internal helper)
    fn check_key(&self, user_id: &str, object_id: &str) -> SqliteResult<bool> {
        let conn = DB_CONN.lock().unwrap();
        let mut stmt = conn.prepare("SELECT COUNT(*) FROM haystack WHERE user = ?1 AND key = ?2")?;
        let count: i64 = stmt.query_row(params![user_id, object_id], |row| row.get(0))?;
        Ok(count > 0)
    }

    /// Check key non-existence and return appropriate error (internal helper)
    fn check_key_nonexistence(&self, user_id: &str, object_id: &str) -> Result<(), Error> {
        if !self.check_key(user_id, object_id).map_err(actix_web::error::ErrorInternalServerError)? {
            warn!("Key does not exist: {}", object_id);
            return Err(actix_web::error::ErrorNotFound(format!("No data found for key: {}, The key does not exist", object_id)));
        }
        Ok(())
    }
}

impl MetadataStorage for SQLiteMetadataStore {
    fn put_metadata(&self, user_id: &str, object_id: &str, metadata: &Metadata) -> Result<(), Error> {
        let offset_size_bytes = metadata.to_bytes()?;
        
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
        
        let result = stmt.query_row(params![user_id, object_id], |row| {
            let offset_size_list: Vec<u8> = row.get(0)?;
            Ok(offset_size_list)
        });
        
        match result {
            Ok(bytes) => Metadata::from_bytes(&bytes),
            Err(e) => Err(actix_web::error::ErrorInternalServerError(e)),
        }
    }

    fn delete_metadata(&self, user_id: &str, object_id: &str) -> Result<(), Error> {
        let conn = DB_CONN.lock().unwrap();
        conn.execute(
            "DELETE FROM haystack WHERE user = ?1 AND key = ?2",
            params![user_id, object_id],
        ).map_err(actix_web::error::ErrorInternalServerError)?;
        Ok(())
    }

    fn exists(&self, user_id: &str, object_id: &str) -> Result<bool, Error> {
        self.check_key(user_id, object_id)
            .map_err(actix_web::error::ErrorInternalServerError)
    }

    fn update_metadata(&self, user_id: &str, object_id: &str, metadata: &Metadata) -> Result<(), Error> {
        let offset_size_bytes = metadata.to_bytes()?;
        
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
}

/// Legacy Database struct for backward compatibility
/// This wraps the new metadata store to maintain existing API
pub struct Database {
    user: String,
    store: SQLiteMetadataStore,
}

impl Database {
    pub fn new(user: &str) -> Result<Self, Error> {
        Ok(Database {
            user: user.to_string(),
            store: SQLiteMetadataStore::new(),
        })
    }

    pub fn check_key(&self, key: &str) -> SqliteResult<bool> {
        self.store.check_key(&self.user, key)
    }

    pub fn check_key_nonexistance(&self, key: &str) -> Result<(), Error> {
        self.store.check_key_nonexistence(&self.user, key)
    }

    pub fn upload_sql(&self, key: &str, offset_size_bytes: &[u8]) -> SqliteResult<()> {
        let metadata = Metadata::from_bytes(offset_size_bytes)
            .map_err(|_| rusqlite::Error::InvalidColumnType(0, "metadata".to_string(), rusqlite::types::Type::Blob))?;
        
        self.store.put_metadata(&self.user, key, &metadata)
            .map_err(|_| rusqlite::Error::InvalidColumnType(0, "put_metadata".to_string(), rusqlite::types::Type::Blob))?;
        
        Ok(())
    }

    pub fn get_offset_size_lists(&self, key: &str) -> SqliteResult<Vec<u8>> {
        let metadata = self.store.get_metadata(&self.user, key)
            .map_err(|_| rusqlite::Error::QueryReturnedNoRows)?;
        
        metadata.to_bytes()
            .map_err(|_| rusqlite::Error::InvalidColumnType(0, "to_bytes".to_string(), rusqlite::types::Type::Blob))
    }

    pub fn delete_from_db(&self, key: &str) -> Result<(), Error> {
        self.store.delete_metadata(&self.user, key)
    }

    pub fn update_key_from_db(&self, old_key: &str, new_key: &str) -> Result<(), Error> {
        self.store.update_object_id(&self.user, old_key, new_key)
    }

    pub fn update_file_db(&self, key: &str, offset_size_bytes: &[u8]) -> Result<(), Error> {
        let metadata = Metadata::from_bytes(offset_size_bytes)?;
        self.store.update_metadata(&self.user, key, &metadata)
    }

    pub fn append_sql(&self, key: &str, offset_size_bytes: &[u8]) -> Result<(), Error> {
        let metadata = Metadata::from_bytes(offset_size_bytes)?;
        self.store.update_metadata(&self.user, key, &metadata)
    }
}