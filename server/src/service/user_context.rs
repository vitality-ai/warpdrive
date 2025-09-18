//! User context structure for handling user-related information

use serde::{Deserialize, Serialize};

/// User context containing all user-related information
/// This struct makes it easy to add new fields without changing function signatures
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UserContext {
    /// User ID
    pub user_id: String,
    /// Bucket name (defaults to "default" if not specified)
    pub bucket: String,
    /// Optional additional metadata that can be extended in the future
    pub metadata: std::collections::HashMap<String, String>,
}

impl UserContext {
    /// Create a new UserContext with default bucket
    pub fn new(user_id: String) -> Self {
        Self {
            bucket: "default".to_string(),
            metadata: std::collections::HashMap::new(),
            user_id,
        }
    }
    
    /// Create a new UserContext with custom bucket
    pub fn with_bucket(user_id: String, bucket: String) -> Self {
        Self {
            bucket,
            metadata: std::collections::HashMap::new(),
            user_id,
        }
    }
    
    /// Set a metadata field
    pub fn set_metadata(&mut self, key: String, value: String) {
        self.metadata.insert(key, value);
    }
    
    /// Get a metadata field
    pub fn get_metadata(&self, key: &str) -> Option<&String> {
        self.metadata.get(key)
    }
}

impl Default for UserContext {
    fn default() -> Self {
        Self::new("default_user".to_string())
    }
}
