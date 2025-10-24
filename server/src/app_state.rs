//! Application State Management
//! 
//! This module provides the application state that contains all services
//! and their dependencies, following the dependency injection pattern.

use std::sync::Arc;
use actix_web::web;
use log::{info, warn};

use crate::storage::{Storage, local_store::LocalXFSBinaryStore, mock_store::MockBinaryStore};
use crate::metadata::{MetadataStorage, sqlite_store::SQLiteMetadataStore, mock_store::MockMetadataStore};
use crate::service::storage_service::StorageService;
use crate::service::metadata_service::MetadataService;
use crate::config::{AppConfig, StorageBackend, MetadataBackend};

/// Application state containing all services and their dependencies
#[derive(Clone)]
pub struct AppState {
    pub storage_service: Arc<StorageService>,
    pub metadata_service: Arc<MetadataService>,
    pub config: AppConfig,
}

impl AppState {
    /// Create a new application state with services configured from YAML config
    pub fn new() -> Self {
        let config = AppConfig::load().expect("Failed to load configuration");
        Self::from_config(config)
    }

    /// Create application state from configuration
    pub fn from_config(config: AppConfig) -> Self {
        info!("Initializing application state with configuration");
        
        // Create storage backend based on configuration
        let storage_backend: Arc<dyn Storage> = {
            match config.storage.backend {
                StorageBackend::LocalXFS => {
                    info!("Using local XFS storage backend with base_path: {}, temp_path: {}", 
                          config.storage.base_path, config.storage.temp_path);
                    Arc::new(LocalXFSBinaryStore::new(Some(&config.storage)))
                },
                StorageBackend::Mock => {
                    info!("Using mock storage backend");
                    Arc::new(MockBinaryStore::new())
                }
            }
        };

        // Create metadata backend based on configuration
        let metadata_backend: Arc<dyn MetadataStorage> = {
            match config.metadata.backend {
                MetadataBackend::SQLite => {
                    info!("Using SQLite metadata backend with db_path: {}, pool_size: {}, wal_mode: {}", 
                          config.metadata.db_path, config.metadata.pool_size, config.metadata.wal_mode);
                    Arc::new(SQLiteMetadataStore::new(Some(&config.metadata)))
                },
                MetadataBackend::Mock => {
                    info!("Using mock metadata backend");
                    Arc::new(MockMetadataStore::new())
                }
            }
        };

        // Create services with injected dependencies
        let storage_service = Arc::new(StorageService::new(storage_backend));
        let metadata_service = Arc::new(MetadataService::new(metadata_backend));

        info!("Application state initialized successfully");
        Self {
            storage_service,
            metadata_service,
            config,
        }
    }

    /// Create application state for testing with mock backends
    pub fn new_for_testing() -> Self {
        let config = AppConfig::default();
        let storage_backend: Arc<dyn Storage> = Arc::new(MockBinaryStore::new());
        let metadata_backend: Arc<dyn MetadataStorage> = Arc::new(MockMetadataStore::new());

        let storage_service = Arc::new(StorageService::new(storage_backend));
        let metadata_service = Arc::new(MetadataService::new(metadata_backend));

        Self {
            storage_service,
            metadata_service,
            config,
        }
    }
}

/// Helper function to extract app state from Actix-web data
pub fn extract_app_state(data: &web::Data<AppState>) -> &AppState {
    data.as_ref()
}

