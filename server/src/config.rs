//! config.rs
//! 
//! Configuration support for metadata storage backend selection.

use std::env;
use std::sync::Arc;

use crate::metadata::MetadataStorage;
use crate::sqlite_store::SQLiteMetadataStore;
use crate::mock_store::MockMetadataStore;

/// Metadata storage backend types
#[derive(Debug, Clone, PartialEq)]
pub enum MetadataBackend {
    SQLite,
    Mock,
}

impl Default for MetadataBackend {
    fn default() -> Self {
        Self::SQLite
    }
}

impl std::str::FromStr for MetadataBackend {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "sqlite" => Ok(Self::SQLite),
            "mock" => Ok(Self::Mock),
            _ => Err(format!("Unknown metadata backend: {}. Valid options: sqlite, mock", s)),
        }
    }
}

/// Configuration for metadata storage
#[derive(Debug, Clone)]
pub struct MetadataConfig {
    pub backend: MetadataBackend,
}

impl Default for MetadataConfig {
    fn default() -> Self {
        Self {
            backend: MetadataBackend::default(),
        }
    }
}

impl MetadataConfig {
    /// Create configuration from environment variables
    pub fn from_env() -> Self {
        let backend = env::var("METADATA_BACKEND")
            .unwrap_or_else(|_| "sqlite".to_string())
            .parse()
            .unwrap_or_else(|err| {
                log::warn!("Invalid METADATA_BACKEND value: {}. Using default SQLite.", err);
                MetadataBackend::default()
            });

        Self { backend }
    }

    /// Create a metadata storage instance based on configuration
    pub fn create_metadata_store(&self) -> Arc<dyn MetadataStorage> {
        match self.backend {
            MetadataBackend::SQLite => Arc::new(SQLiteMetadataStore::new()),
            MetadataBackend::Mock => Arc::new(MockMetadataStore::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metadata_backend_from_str() {
        assert_eq!("sqlite".parse::<MetadataBackend>().unwrap(), MetadataBackend::SQLite);
        assert_eq!("SQLite".parse::<MetadataBackend>().unwrap(), MetadataBackend::SQLite);
        assert_eq!("SQLITE".parse::<MetadataBackend>().unwrap(), MetadataBackend::SQLite);
        assert_eq!("mock".parse::<MetadataBackend>().unwrap(), MetadataBackend::Mock);
        assert_eq!("Mock".parse::<MetadataBackend>().unwrap(), MetadataBackend::Mock);
        assert_eq!("MOCK".parse::<MetadataBackend>().unwrap(), MetadataBackend::Mock);
        
        assert!("invalid".parse::<MetadataBackend>().is_err());
    }

    #[test]
    fn test_metadata_config_default() {
        let config = MetadataConfig::default();
        assert_eq!(config.backend, MetadataBackend::SQLite);
    }

    #[test]
    fn test_metadata_config_create_store() {
        let config = MetadataConfig { backend: MetadataBackend::SQLite };
        let store = config.create_metadata_store();
        // We can't easily test the concrete type, but we can ensure it implements MetadataStorage
        assert!(store.exists("test", "test").is_ok());

        let config = MetadataConfig { backend: MetadataBackend::Mock };
        let store = config.create_metadata_store();
        assert!(store.exists("test", "test").is_ok());
    }
}