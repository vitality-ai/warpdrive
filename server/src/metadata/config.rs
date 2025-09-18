//! Configuration for metadata storage backends

use crate::metadata::{MetadataStorage, sqlite_store::SQLiteMetadataStore, mock_store::MockMetadataStore};
use std::sync::Arc;
use std::env;
use log::{info, warn};

/// Available metadata storage backends
#[derive(Debug, Clone, PartialEq)]
pub enum MetadataBackend {
    SQLite,
    Mock,
}

impl Default for MetadataBackend {
    fn default() -> Self {
        MetadataBackend::SQLite
    }
}

impl std::str::FromStr for MetadataBackend {
    type Err = String;
    
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "sqlite" => Ok(MetadataBackend::SQLite),
            "mock" => Ok(MetadataBackend::Mock),
            _ => Err(format!("Unknown metadata backend: {}", s))
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
    /// Create a new metadata configuration from environment variables
    pub fn from_env() -> Self {
        let backend = match env::var("METADATA_BACKEND") {
            Ok(backend_str) => {
                match backend_str.parse::<MetadataBackend>() {
                    Ok(backend) => {
                        info!("Using metadata backend from environment: {:?}", backend);
                        backend
                    }
                    Err(e) => {
                        warn!("Invalid metadata backend in environment: {}. Using default SQLite.", e);
                        MetadataBackend::default()
                    }
                }
            }
            Err(_) => {
                info!("No metadata backend specified in environment, using default SQLite");
                MetadataBackend::default()
            }
        };
        
        Self { backend }
    }
    
    /// Create a metadata storage instance based on the configuration
    pub fn create_store(&self) -> Arc<dyn MetadataStorage> {
        match self.backend {
            MetadataBackend::SQLite => {
                info!("Creating SQLite metadata store");
                Arc::new(SQLiteMetadataStore::new())
            }
            MetadataBackend::Mock => {
                info!("Creating Mock metadata store");
                Arc::new(MockMetadataStore::new())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[test]
    fn test_metadata_backend_from_str() {
        assert_eq!("sqlite".parse::<MetadataBackend>().unwrap(), MetadataBackend::SQLite);
        assert_eq!("SQLite".parse::<MetadataBackend>().unwrap(), MetadataBackend::SQLite);
        assert_eq!("mock".parse::<MetadataBackend>().unwrap(), MetadataBackend::Mock);
        assert_eq!("MOCK".parse::<MetadataBackend>().unwrap(), MetadataBackend::Mock);
        
        assert!("invalid".parse::<MetadataBackend>().is_err());
    }
    
    #[test]
    fn test_metadata_config_default() {
        let config = MetadataConfig::default();
        assert_eq!(config.backend, MetadataBackend::SQLite);
    }
    
    #[test]
    fn test_metadata_config_from_env() {
        // Test with environment variable set
        env::set_var("METADATA_BACKEND", "mock");
        let config = MetadataConfig::from_env();
        assert_eq!(config.backend, MetadataBackend::Mock);
        
        // Test with invalid environment variable
        env::set_var("METADATA_BACKEND", "invalid");
        let config = MetadataConfig::from_env();
        assert_eq!(config.backend, MetadataBackend::SQLite);
        
        // Clean up
        env::remove_var("METADATA_BACKEND");
        let config = MetadataConfig::from_env();
        assert_eq!(config.backend, MetadataBackend::SQLite);
    }
    
    #[test]
    fn test_create_store() {
        // Test SQLite store creation
        let config = MetadataConfig { backend: MetadataBackend::SQLite };
        let store = config.create_store();
        // Verify store creation succeeds and we can call methods on it
        let result = store.list_objects("test_user", "default");
        assert!(result.is_ok());
        
        // Test Mock store creation
        let config = MetadataConfig { backend: MetadataBackend::Mock };
        let store = config.create_store();
        // Verify store creation succeeds and we can call methods on it
        let result = store.list_objects("test_user", "default");
        assert!(result.is_ok());
    }
}