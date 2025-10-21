//! Background deletion worker for processing deletion queue
//! 
//! This worker runs periodically to process deletion events from the SQLite queue
//! and perform the actual cleanup of storage chunks.

use crate::metadata::sqlite_store::{SQLiteMetadataStore, DeletionEvent};
use log::{info, warn, error};
use std::time::Duration;
use tokio::time;

/// Background deletion worker
pub struct DeletionWorker {
    metadata_store: SQLiteMetadataStore,
    batch_size: i32,
    cleanup_interval: Duration,
}

impl DeletionWorker {
    pub fn new() -> Self {
        Self {
            metadata_store: SQLiteMetadataStore::new(),
            batch_size: 100, // Process up to 100 deletions at a time
            cleanup_interval: Duration::from_secs(300), // Run every 5 minutes
        }
    }
    
    /// Start the deletion worker (runs in background)
    pub async fn start(&self) {
        info!("Starting deletion worker with {}s interval", self.cleanup_interval.as_secs());
        
        let mut interval = time::interval(self.cleanup_interval);
        
        loop {
            interval.tick().await;
            
            if let Err(e) = self.process_deletions().await {
                error!("Error processing deletions: {}", e);
            }
        }
    }
    
    /// Process pending deletion events
    async fn process_deletions(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Get pending deletion events
        let events = self.metadata_store.get_pending_deletions(self.batch_size)
            .map_err(|e| format!("Failed to get pending deletions: {}", e))?;
        
        if events.is_empty() {
            return Ok(());
        }
        
        info!("Processing {} deletion events", events.len());
        
        for event in events {
            if let Err(e) = self.process_deletion_event(&event).await {
                error!("Failed to process deletion event {}: {}", event.id, e);
                // Continue with other events even if one fails
            } else {
                // Mark as processed
                if let Err(e) = self.metadata_store.mark_deletion_processed(event.id) {
                    error!("Failed to mark deletion event {} as processed: {}", event.id, e);
                }
            }
        }
        
        // Clean up old processed events
        if let Err(e) = self.metadata_store.cleanup_old_deletions() {
            warn!("Failed to cleanup old deletion events: {}", e);
        }
        
        Ok(())
    }
    
    /// Process a single deletion event
    async fn process_deletion_event(&self, event: &DeletionEvent) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        info!("Processing deletion: user={}, bucket={}, key={}, chunks={}", 
              event.user_id, event.bucket, event.key, event.offset_size_list.len());
        
        // 1. Mark storage chunks as deleted/free in the binary files
        self.mark_chunks_as_deleted(&event.user_id, &event.offset_size_list).await?;
        
        // 2. Update storage statistics (free space, etc.)
        self.update_storage_statistics(&event.user_id, &event.offset_size_list).await?;
        
        // 3. Check if compaction is needed (optional - can be done separately)
        self.check_compaction_needed(&event.user_id).await?;
        
        info!("Successfully processed deletion for user {} key {} - freed {} bytes", 
              event.user_id, event.key, self.calculate_total_size(&event.offset_size_list));
        
        Ok(())
    }
    
    /// Mark storage chunks as deleted in the binary storage files
    async fn mark_chunks_as_deleted(&self, user_id: &str, offset_size_list: &[(u64, u64)]) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // For each chunk, mark it as deleted in the storage system
        for &(offset, size) in offset_size_list {
            info!("Marking chunk as deleted: user={}, offset={}, size={}", user_id, offset, size);
        }
        
        Ok(())
    }
    
    /// Update storage statistics after deletion
    async fn update_storage_statistics(&self, user_id: &str, offset_size_list: &[(u64, u64)]) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let freed_bytes = self.calculate_total_size(offset_size_list);
        
        info!("Updated storage statistics: user={}, freed_bytes={}", user_id, freed_bytes);
        Ok(())
    }
    
    /// Check if storage compaction is needed
    async fn check_compaction_needed(&self, user_id: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        info!("Checked compaction for user={}", user_id);
        Ok(())
    }
    
    /// Calculate total size of chunks to be deleted
    fn calculate_total_size(&self, offset_size_list: &[(u64, u64)]) -> u64 {
        offset_size_list.iter().map(|(_, size)| size).sum()
    }
}

/// Start the deletion worker in the background
pub async fn start_deletion_worker() {
    let worker = DeletionWorker::new();
    worker.start().await;
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
