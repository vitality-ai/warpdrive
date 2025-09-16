//! Configuration for binary storage backends

use crate::storage::{Storage, local_store::LocalXFSBinaryStore, mock_store::MockBinaryStore};
use std::sync::Arc;
use std::env;
use log::{info, warn};

/// Available binary storage backends
#[derive(Debug, Clone, PartialEq)]
pub enum StorageBackend {
    LocalXFS,
    Mock,
}

impl Default for StorageBackend {
    fn default() -> Self {
        StorageBackend::LocalXFS
    }
}

impl std::str::FromStr for StorageBackend {
    type Err = String;
    
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "localxfs" | "local" | "xfs" => Ok(StorageBackend::LocalXFS),
            "mock" => Ok(StorageBackend::Mock),
            _ => Err(format!("Unknown storage backend: {}", s))
        }
    }
}

/// Configuration for binary storage
#[derive(Debug, Clone)]
pub struct StorageConfig {
    pub backend: StorageBackend,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            backend: StorageBackend::default(),
        }
    }
}

impl StorageConfig {
    /// Create a new storage configuration from environment variables
    pub fn from_env() -> Self {
        let backend = match env::var("STORAGE_BACKEND") {
            Ok(backend_str) => {
                match backend_str.parse::<StorageBackend>() {
                    Ok(backend) => {
                        info!("Using storage backend from environment: {:?}", backend);
                        backend
                    }
                    Err(e) => {
                        warn!("Invalid storage backend in environment: {}. Using default LocalXFS.", e);
                        StorageBackend::default()
                    }
                }
            }
            Err(_) => {
                info!("No storage backend specified in environment, using default LocalXFS");
                StorageBackend::default()
            }
        };
        
        Self { backend }
    }
    
    /// Create a storage instance based on the configuration
    pub fn create_store(&self) -> Arc<dyn Storage> {
        match self.backend {
            StorageBackend::LocalXFS => Arc::new(LocalXFSBinaryStore::new()),
            StorageBackend::Mock => Arc::new(MockBinaryStore::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_storage_backend_from_str() {
        assert_eq!("localxfs".parse::<StorageBackend>().unwrap(), StorageBackend::LocalXFS);
        assert_eq!("LocalXFS".parse::<StorageBackend>().unwrap(), StorageBackend::LocalXFS);
        assert_eq!("local".parse::<StorageBackend>().unwrap(), StorageBackend::LocalXFS);
        assert_eq!("xfs".parse::<StorageBackend>().unwrap(), StorageBackend::LocalXFS);
        assert_eq!("mock".parse::<StorageBackend>().unwrap(), StorageBackend::Mock);
        assert_eq!("MOCK".parse::<StorageBackend>().unwrap(), StorageBackend::Mock);
        
        assert!("invalid".parse::<StorageBackend>().is_err());
    }
    
    #[test]
    fn test_storage_config_default() {
        let config = StorageConfig::default();
        assert_eq!(config.backend, StorageBackend::LocalXFS);
    }
    
    #[test]
    fn test_create_store() {
        let local_config = StorageConfig { backend: StorageBackend::LocalXFS };
        let mock_config = StorageConfig { backend: StorageBackend::Mock };
        
        let _local_store = local_config.create_store();
        let _mock_store = mock_config.create_store();
        
        // Just verify they can be created without errors
    }
}