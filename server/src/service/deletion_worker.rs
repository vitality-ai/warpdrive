//! Background deletion worker for processing deletion queue
//! 
//! This worker runs periodically to process deletion events, free up space,
//! and trigger compaction when there's enough free space at the top of the file.
//! Since we're append-only, if there's enough free space at top we compact,
//! otherwise we leave holes until compaction becomes easier.

use crate::service::metadata_service::MetadataService;
use crate::service::storage_service::StorageService;
use crate::service::user_context::UserContext;
use crate::metadata::sqlite_store::DeletionEvent;
use log::{info, warn, error};
use std::time::Duration;
use tokio::time;

/// Background deletion worker
pub struct DeletionWorker {
    batch_size: i32,
    cleanup_interval: Duration,
}

impl DeletionWorker {
    pub fn new() -> Self {
        Self {
            batch_size: 100, // Process up to 100 deletions at a time
            cleanup_interval: Duration::from_secs(300), // Run every 5 minutes
        }
    }
    
    /// Start the deletion worker as a background task (non-blocking)
    pub fn start_background(self) -> tokio::task::JoinHandle<()> {
        info!("Starting deletion worker with {}s interval", self.cleanup_interval.as_secs());
        
        tokio::spawn(async move {
            let mut interval = time::interval(self.cleanup_interval);
            
            loop {
                interval.tick().await;
                
                if let Err(e) = self.process_deletions().await {
                    error!("Error processing deletions: {}", e);
                }
            }
        })
    }
    
    /// Process pending deletion events
    async fn process_deletions(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Get pending deletion events through metadata service
        let metadata_service = match MetadataService::new("system") {
            Ok(service) => service,
            Err(e) => {
                error!("Failed to create metadata service: {}", e);
                return Err(format!("Failed to create metadata service: {}", e).into());
            }
        };
        
        let events = match metadata_service.get_pending_deletions(self.batch_size) {
            Ok(events) => events,
            Err(e) => {
                error!("Failed to get pending deletions: {}", e);
                return Err(format!("Failed to get pending deletions: {}", e).into());
            }
        };
        
        if events.is_empty() {
            return Ok(());
        }
        
        info!("Processing {} deletion events", events.len());
        
        for event in events {
            if let Err(e) = self.process_deletion_event(&event).await {
                error!("Failed to process deletion event {}: {}", event.id, e);
                // Continue with other events even if one fails
            } else {
                // Mark as processed through metadata service
                if let Err(e) = metadata_service.mark_deletion_processed(event.id) {
                    error!("Failed to mark deletion event {} as processed: {}", event.id, e);
                }
            }
        }
        
        // Clean up old processed events
        if let Err(e) = metadata_service.cleanup_old_deletions() {
            warn!("Failed to cleanup old deletion events: {}", e);
        }
        
        Ok(())
    }
    
    /// Process a single deletion event
    async fn process_deletion_event(&self, event: &DeletionEvent) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        info!("Processing deletion: user={}, bucket={}, key={}, chunks={}", 
              event.user_id, event.bucket, event.key, event.offset_size_list.len());

        // Create user context for this deletion
        let context = UserContext::with_bucket(event.user_id.clone(), event.bucket.clone());

        // Use storage service to delete the actual chunks (marks them as free)
        let storage_service = StorageService::new();
        if let Err(e) = storage_service.delete_chunks(&context, &event.offset_size_list) {
            return Err(format!("Failed to delete chunks: {}", e).into());
        }

        let freed_bytes = self.calculate_total_size(&event.offset_size_list);
        info!("Freed {} bytes for user {} bucket {}", freed_bytes, event.user_id, event.bucket);

        // Check if we should trigger compaction
        // For now, we'll leave holes until there's enough free space at top of file
        // This is a simplified approach - in the future we can add compaction logic here
        // that checks if free space at top >= some threshold, then compact
        
        Ok(())
    }
    
    /// Calculate total size of chunks to be deleted
    fn calculate_total_size(&self, offset_size_list: &[(u64, u64)]) -> u64 {
        offset_size_list.iter().map(|(_, size)| size).sum()
    }
}

/// Start the deletion worker as a background task (non-blocking)
pub fn start_deletion_worker() -> tokio::task::JoinHandle<()> {
    let worker = DeletionWorker::new();
    worker.start_background()
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[tokio::test]
    async fn test_deletion_worker_creation() {
        let worker = DeletionWorker::new();
        assert_eq!(worker.batch_size, 100);
        assert_eq!(worker.cleanup_interval.as_secs(), 300);
    }
}
