//! Application Configuration
//! 
//! This module provides configuration management for the application,
//! supporting YAML configuration files with sensible defaults.

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::fs;
use log::{info, warn};

/// Storage backend types
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum StorageBackend {
    LocalXFS,
    Mock,
}

impl Default for StorageBackend {
    fn default() -> Self {
        StorageBackend::LocalXFS
    }
}

/// Metadata backend types
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum MetadataBackend {
    SQLite,
    Mock,
}

impl Default for MetadataBackend {
    fn default() -> Self {
        MetadataBackend::SQLite
    }
}

/// Main application configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// Server configuration
    pub server: ServerConfig,
    /// Storage configuration
    pub storage: StorageConfig,
    /// Metadata configuration
    pub metadata: MetadataConfig,
    /// Deletion worker configuration
    pub deletion: DeletionConfig,
    /// Logging configuration
    pub logging: LoggingConfig,
}

/// Server configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    /// Server host
    pub host: String,
    /// Server port
    pub port: u16,
    /// Number of worker threads
    pub workers: usize,
    /// Maximum payload size in bytes
    pub max_payload_size: u64,
}

/// Storage backend configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    /// Storage backend type
    pub backend: StorageBackend,
    /// Base path for storage files
    pub base_path: String,
    /// Temporary path for temporary files
    pub temp_path: String,
}

/// Metadata backend configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetadataConfig {
    /// Metadata backend type
    pub backend: MetadataBackend,
    /// Database file path
    pub db_path: String,
    /// Connection pool size
    pub pool_size: u32,
    /// Enable WAL mode
    pub wal_mode: bool,
}

/// Deletion worker configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeletionConfig {
    /// Enable deletion worker
    pub enabled: bool,
    /// Cleanup interval in seconds
    pub cleanup_interval: u64,
    /// Batch size for processing deletions
    pub batch_size: usize,
    /// Number of retry attempts
    pub retry_attempts: u32,
    /// Retry delay in seconds
    pub retry_delay: u64,
}

/// Logging configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    /// Path to log configuration file
    pub config_file: String,
}

impl AppConfig {
    /// Load configuration from file, use defaults if not found
    pub fn load() -> Result<Self, Box<dyn std::error::Error>> {
        let config_path = "config.yaml";
        if Path::new(config_path).exists() {
            let content = fs::read_to_string(config_path)?;
            let config: AppConfig = serde_yaml::from_str(&content)?;
            info!("Loaded configuration from {}", config_path);
            Ok(config)
        } else {
            warn!("Config file not found, using defaults");
            Ok(Self::default())
        }
    }

    /// Create default configuration
    pub fn default() -> Self {
        Self {
            server: ServerConfig {
                host: "127.0.0.1".to_string(),
                port: 9710,
                workers: 4,
                max_payload_size: 1073741824, // 1GB
            },
            storage: StorageConfig {
                backend: StorageBackend::LocalXFS,
                base_path: "./data/storage".to_string(),
                temp_path: "./data/temp".to_string(),
            },
            metadata: MetadataConfig {
                backend: MetadataBackend::SQLite,
                db_path: "./data/metadata.db".to_string(),
                pool_size: 10,
                wal_mode: true,
            },
            deletion: DeletionConfig {
                enabled: true,
                cleanup_interval: 300, // 5 minutes
                batch_size: 100,
                retry_attempts: 3,
                retry_delay: 5,
            },
            logging: LoggingConfig {
                config_file: "server_log.yaml".to_string(),
            },
        }
    }
}