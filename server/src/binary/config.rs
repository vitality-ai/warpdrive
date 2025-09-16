//! Configuration for binary storage backends

use crate::binary::{BinaryStorage, local_xfs_store::LocalXFSBinaryStore, mock_store::MockBinaryStore};
use std::sync::Arc;
use std::env;
use log::{info, warn};

/// Available binary storage backends
#[derive(Debug, Clone, PartialEq)]
pub enum BinaryBackend {
    LocalXFS,
    Mock,
}

impl Default for BinaryBackend {
    fn default() -> Self {
        BinaryBackend::LocalXFS
    }
}

impl std::str::FromStr for BinaryBackend {
    type Err = String;
    
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "localxfs" | "local_xfs" | "xfs" => Ok(BinaryBackend::LocalXFS),
            "mock" => Ok(BinaryBackend::Mock),
            _ => Err(format!("Unknown binary storage backend: {}", s))
        }
    }
}

/// Configuration for binary storage
#[derive(Debug, Clone)]
pub struct BinaryConfig {
    pub backend: BinaryBackend,
}

impl Default for BinaryConfig {
    fn default() -> Self {
        Self {
            backend: BinaryBackend::default(),
        }
    }
}

impl BinaryConfig {
    /// Create a new binary configuration from environment variables
    pub fn from_env() -> Self {
        let backend = match env::var("BINARY_BACKEND") {
            Ok(backend_str) => {
                match backend_str.parse::<BinaryBackend>() {
                    Ok(backend) => {
                        info!("Using binary storage backend from environment: {:?}", backend);
                        backend
                    }
                    Err(e) => {
                        warn!("Invalid binary storage backend in environment: {}. Using default LocalXFS.", e);
                        BinaryBackend::default()
                    }
                }
            }
            Err(_) => {
                info!("No binary storage backend specified in environment, using default LocalXFS");
                BinaryBackend::default()
            }
        };
        
        Self { backend }
    }

    /// Create a binary storage instance based on the configuration
    pub fn create_store(&self) -> Arc<dyn BinaryStorage> {
        match self.backend {
            BinaryBackend::LocalXFS => {
                info!("Creating LocalXFS binary storage backend");
                Arc::new(LocalXFSBinaryStore::new())
            }
            BinaryBackend::Mock => {
                info!("Creating Mock binary storage backend");
                Arc::new(MockBinaryStore::new())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[test]
    fn test_binary_backend_from_str() {
        assert_eq!("localxfs".parse::<BinaryBackend>().unwrap(), BinaryBackend::LocalXFS);
        assert_eq!("local_xfs".parse::<BinaryBackend>().unwrap(), BinaryBackend::LocalXFS);
        assert_eq!("xfs".parse::<BinaryBackend>().unwrap(), BinaryBackend::LocalXFS);
        assert_eq!("LocalXFS".parse::<BinaryBackend>().unwrap(), BinaryBackend::LocalXFS);
        assert_eq!("mock".parse::<BinaryBackend>().unwrap(), BinaryBackend::Mock);
        assert_eq!("MOCK".parse::<BinaryBackend>().unwrap(), BinaryBackend::Mock);
        
        assert!("invalid".parse::<BinaryBackend>().is_err());
    }
    
    #[test]
    fn test_binary_config_default() {
        let config = BinaryConfig::default();
        assert_eq!(config.backend, BinaryBackend::LocalXFS);
    }
    
    #[test]
    fn test_binary_config_from_env() {
        // Test with valid backend
        env::set_var("BINARY_BACKEND", "mock");
        let config = BinaryConfig::from_env();
        assert_eq!(config.backend, BinaryBackend::Mock);
        
        // Test with invalid backend
        env::set_var("BINARY_BACKEND", "invalid");
        let config = BinaryConfig::from_env();
        assert_eq!(config.backend, BinaryBackend::LocalXFS); // Should fall back to default
        
        // Test with no environment variable
        env::remove_var("BINARY_BACKEND");
        let config = BinaryConfig::from_env();
        assert_eq!(config.backend, BinaryBackend::LocalXFS);
    }

    #[test]
    fn test_create_store() {
        let config = BinaryConfig { backend: BinaryBackend::LocalXFS };
        let store = config.create_store();
        // Just test that we can create the store without panicking
        assert!(store.as_ref() as *const dyn BinaryStorage as *const u8 != std::ptr::null());
        
        let config = BinaryConfig { backend: BinaryBackend::Mock };
        let store = config.create_store();
        // Just test that we can create the store without panicking
        assert!(store.as_ref() as *const dyn BinaryStorage as *const u8 != std::ptr::null());
    }
}