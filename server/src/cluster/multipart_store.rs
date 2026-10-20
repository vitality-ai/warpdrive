// Author: Tejus Chandrashekar
//! Multipart-upload bookkeeping for the cluster path, Bitcask-style --
//! the same append-only-log-plus-in-memory-index pattern as
//! `shard_meta_store.rs`/`location_store.rs`, applied to upload/part
//! metadata instead of shard offsets or placement pins.
//!
//! **Scope, deliberately narrower than single-node's multipart** (see
//! `coordinator.rs`'s multipart functions for the full picture): only
//! plain (non-content-dependent) final objects are supported. A part's
//! bytes are stored as an ordinary plain EC object under a synthetic key
//! (same NUL-byte key-space-disjointness trick `packed.rs::stripe_key`
//! already uses), so this store only ever tracks bookkeeping -- upload
//! metadata and the ordered list of (part_number, etag, size) -- never
//! raw bytes itself.
//!
//! **Per-node, not replicated** (see `ClusterState::multipart_store`'s
//! own doc for the full reasoning): a part's *bytes* are fully
//! replicated via the ordinary plain-object write path, but this store's
//! bookkeeping is not. A real deployment needs sticky per-`upload_id`
//! routing across the lifetime of one multipart upload.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MultipartUpload {
    pub upload_id: String,
    pub user_id: String,
    pub bucket: String,
    pub key: String,
    pub content_type: Option<String>,
    #[serde(default)]
    pub user_metadata: HashMap<String, String>,
    pub initiated_at: String,
    /// "in_progress" | "completed" | "aborted".
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MultipartPart {
    pub part_number: i32,
    pub etag: String,
    pub size: u64,
}

pub trait MultipartStore: Send + Sync {
    fn create_upload(&self, upload: MultipartUpload) -> io::Result<()>;
    fn get_upload(&self, upload_id: &str) -> Option<MultipartUpload>;
    fn set_status(&self, upload_id: &str, status: &str) -> io::Result<()>;
    /// Removes the upload's bookkeeping entry (not its parts -- see
    /// `delete_parts`, called separately so a caller can choose to list
    /// parts one last time before tearing everything down).
    fn delete_upload(&self, upload_id: &str) -> io::Result<()>;
    fn put_part(&self, upload_id: &str, part: MultipartPart) -> io::Result<()>;
    /// Sorted by `part_number`.
    fn list_parts(&self, upload_id: &str) -> Vec<MultipartPart>;
    fn delete_parts(&self, upload_id: &str) -> io::Result<()>;
}

/// One log entry: either an upload bookkeeping row or one part row,
/// `None` as a tombstone for either -- same JSON-lines-with-tombstone
/// convention as every other Bitcask store in this project (#152: JSON
/// escapes arbitrary byte content in `upload_id`/keys as a matter of
/// course, unlike a raw delimited format).
#[derive(Debug, Serialize, Deserialize)]
enum LogEntry {
    Upload(String, Option<MultipartUpload>),
    Part(String, i32, Option<MultipartPart>),
}

pub struct BitcaskMultipartStore {
    log_path: PathBuf,
    log_file: RwLock<File>,
    uploads: RwLock<HashMap<String, MultipartUpload>>,
    parts: RwLock<HashMap<String, HashMap<i32, MultipartPart>>>,
    /// #155's same reasoning: one lock held across append-then-index-update
    /// in every mutating method, so a restart's log replay can never
    /// disagree with what the live index had.
    write_lock: std::sync::Mutex<()>,
}

impl BitcaskMultipartStore {
    pub fn open(log_path: impl AsRef<Path>) -> io::Result<Self> {
        let log_path = log_path.as_ref().to_path_buf();
        if let Some(parent) = log_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut uploads: HashMap<String, MultipartUpload> = HashMap::new();
        let mut parts: HashMap<String, HashMap<i32, MultipartPart>> = HashMap::new();

        if log_path.exists() {
            let f = File::open(&log_path)?;
            let lines: Vec<String> = BufReader::new(f).lines().collect::<io::Result<_>>()?;
            let last_idx = lines.len().saturating_sub(1);
            for (i, line) in lines.iter().enumerate() {
                if line.is_empty() {
                    continue;
                }
                match serde_json::from_str::<LogEntry>(line) {
                    Ok(LogEntry::Upload(id, Some(u))) => {
                        uploads.insert(id, u);
                    }
                    Ok(LogEntry::Upload(id, None)) => {
                        uploads.remove(&id);
                    }
                    Ok(LogEntry::Part(upload_id, part_number, Some(p))) => {
                        parts.entry(upload_id).or_default().insert(part_number, p);
                    }
                    Ok(LogEntry::Part(upload_id, part_number, None)) => {
                        if let Some(m) = parts.get_mut(&upload_id) {
                            m.remove(&part_number);
                        }
                    }
                    // Same reasoning as every other Bitcask store here
                    // (#152): only the last line can be a torn write from
                    // a crash mid-append.
                    Err(e) if i == last_idx => {
                        log::warn!(
                            "multipart log {}: ignoring malformed trailing line, \
                             likely a torn write from a crash: {e}",
                            log_path.display()
                        );
                    }
                    Err(e) => return Err(io::Error::new(io::ErrorKind::InvalidData, e)),
                }
            }
        }

        let log_file = OpenOptions::new().create(true).append(true).open(&log_path)?;
        Ok(Self {
            log_path,
            log_file: RwLock::new(log_file),
            uploads: RwLock::new(uploads),
            parts: RwLock::new(parts),
            write_lock: std::sync::Mutex::new(()),
        })
    }

    pub fn path(&self) -> &Path {
        &self.log_path
    }

    fn append(&self, entry: &LogEntry) -> io::Result<()> {
        let line = serde_json::to_string(entry).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let mut f = self.log_file.write().unwrap();
        writeln!(f, "{line}")?;
        f.flush()
    }
}

impl MultipartStore for BitcaskMultipartStore {
    fn create_upload(&self, upload: MultipartUpload) -> io::Result<()> {
        let _guard = self.write_lock.lock().unwrap();
        self.append(&LogEntry::Upload(upload.upload_id.clone(), Some(upload.clone())))?;
        self.uploads.write().unwrap().insert(upload.upload_id.clone(), upload);
        Ok(())
    }

    fn get_upload(&self, upload_id: &str) -> Option<MultipartUpload> {
        self.uploads.read().unwrap().get(upload_id).cloned()
    }

    fn set_status(&self, upload_id: &str, status: &str) -> io::Result<()> {
        let _guard = self.write_lock.lock().unwrap();
        let mut uploads = self.uploads.write().unwrap();
        let Some(mut upload) = uploads.get(upload_id).cloned() else {
            return Err(io::Error::new(io::ErrorKind::NotFound, "no such upload"));
        };
        upload.status = status.to_string();
        self.append(&LogEntry::Upload(upload_id.to_string(), Some(upload.clone())))?;
        uploads.insert(upload_id.to_string(), upload);
        Ok(())
    }

    fn delete_upload(&self, upload_id: &str) -> io::Result<()> {
        let _guard = self.write_lock.lock().unwrap();
        self.append(&LogEntry::Upload(upload_id.to_string(), None))?;
        self.uploads.write().unwrap().remove(upload_id);
        Ok(())
    }

    fn put_part(&self, upload_id: &str, part: MultipartPart) -> io::Result<()> {
        let _guard = self.write_lock.lock().unwrap();
        self.append(&LogEntry::Part(upload_id.to_string(), part.part_number, Some(part.clone())))?;
        self.parts.write().unwrap().entry(upload_id.to_string()).or_default().insert(part.part_number, part);
        Ok(())
    }

    fn list_parts(&self, upload_id: &str) -> Vec<MultipartPart> {
        let parts = self.parts.read().unwrap();
        let mut v: Vec<MultipartPart> = parts.get(upload_id).map(|m| m.values().cloned().collect()).unwrap_or_default();
        v.sort_by_key(|p| p.part_number);
        v
    }

    fn delete_parts(&self, upload_id: &str) -> io::Result<()> {
        let _guard = self.write_lock.lock().unwrap();
        let part_numbers: Vec<i32> = self.parts.read().unwrap().get(upload_id).map(|m| m.keys().cloned().collect()).unwrap_or_default();
        for pn in part_numbers {
            self.append(&LogEntry::Part(upload_id.to_string(), pn, None))?;
        }
        self.parts.write().unwrap().remove(upload_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_upload(id: &str) -> MultipartUpload {
        MultipartUpload {
            upload_id: id.to_string(),
            user_id: "u1".to_string(),
            bucket: "b1".to_string(),
            key: "k1".to_string(),
            content_type: Some("text/plain".to_string()),
            user_metadata: HashMap::new(),
            initiated_at: "2026-01-01T00:00:00Z".to_string(),
            status: "in_progress".to_string(),
        }
    }

    #[test]
    fn create_then_get_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let store = BitcaskMultipartStore::open(dir.path().join("mpu.log")).unwrap();
        store.create_upload(sample_upload("up1")).unwrap();
        assert_eq!(store.get_upload("up1").unwrap(), sample_upload("up1"));
    }

    #[test]
    fn set_status_updates_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let store = BitcaskMultipartStore::open(dir.path().join("mpu.log")).unwrap();
        store.create_upload(sample_upload("up1")).unwrap();
        store.set_status("up1", "completed").unwrap();
        assert_eq!(store.get_upload("up1").unwrap().status, "completed");
    }

    #[test]
    fn parts_are_listed_sorted_by_part_number_regardless_of_insert_order() {
        let dir = tempfile::tempdir().unwrap();
        let store = BitcaskMultipartStore::open(dir.path().join("mpu.log")).unwrap();
        store.put_part("up1", MultipartPart { part_number: 2, etag: "e2".into(), size: 20 }).unwrap();
        store.put_part("up1", MultipartPart { part_number: 1, etag: "e1".into(), size: 10 }).unwrap();
        store.put_part("up1", MultipartPart { part_number: 3, etag: "e3".into(), size: 30 }).unwrap();

        let parts = store.list_parts("up1");
        assert_eq!(parts.iter().map(|p| p.part_number).collect::<Vec<_>>(), vec![1, 2, 3]);
    }

    #[test]
    fn delete_upload_and_delete_parts_are_independent() {
        let dir = tempfile::tempdir().unwrap();
        let store = BitcaskMultipartStore::open(dir.path().join("mpu.log")).unwrap();
        store.create_upload(sample_upload("up1")).unwrap();
        store.put_part("up1", MultipartPart { part_number: 1, etag: "e1".into(), size: 10 }).unwrap();

        store.delete_upload("up1").unwrap();
        assert!(store.get_upload("up1").is_none());
        // Parts survive deleting just the upload row -- caller deletes
        // them separately (e.g. after listing them one last time).
        assert_eq!(store.list_parts("up1").len(), 1);

        store.delete_parts("up1").unwrap();
        assert!(store.list_parts("up1").is_empty());
    }

    #[test]
    fn restart_rebuilds_both_indexes_from_log_replay() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("mpu.log");
        {
            let store = BitcaskMultipartStore::open(&log_path).unwrap();
            store.create_upload(sample_upload("up1")).unwrap();
            store.put_part("up1", MultipartPart { part_number: 1, etag: "e1".into(), size: 10 }).unwrap();
            store.put_part("up1", MultipartPart { part_number: 2, etag: "e2".into(), size: 20 }).unwrap();
        }
        let reopened = BitcaskMultipartStore::open(&log_path).unwrap();
        assert_eq!(reopened.get_upload("up1").unwrap(), sample_upload("up1"));
        assert_eq!(reopened.list_parts("up1").len(), 2);
    }

    #[test]
    fn a_malformed_trailing_line_is_quarantined_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("mpu.log");
        {
            let store = BitcaskMultipartStore::open(&log_path).unwrap();
            store.create_upload(sample_upload("up1")).unwrap();
        }
        {
            let mut f = OpenOptions::new().append(true).open(&log_path).unwrap();
            writeln!(f, "{{\"Upload\":[\"up2\",{{\"upload_id\"").unwrap();
        }
        let reopened = BitcaskMultipartStore::open(&log_path).unwrap();
        assert_eq!(reopened.get_upload("up1").unwrap(), sample_upload("up1"));
        assert!(reopened.get_upload("up2").is_none());
    }
}
