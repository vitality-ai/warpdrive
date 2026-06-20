//! Mock implementation of MetadataStorage trait for testing

use crate::metadata::{MetadataStorage, Metadata, ObjectId, BucketStats};
use actix_web::Error;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use chrono::Utc;

/// Mock implementation of MetadataStorage for testing
pub struct MockMetadataStore {
    data: Arc<Mutex<HashMap<String, HashMap<String, HashMap<String, Metadata>>>>>,
    buckets: Arc<Mutex<HashMap<String, HashSet<String>>>>,
}

impl MockMetadataStore {
    pub fn new() -> Self {
        Self {
            data: Arc::new(Mutex::new(HashMap::new())),
            buckets: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn clear(&self) {
        self.data.lock().unwrap().clear();
        self.buckets.lock().unwrap().clear();
    }

    pub fn user_count(&self) -> usize {
        self.data.lock().unwrap().len()
    }

    pub fn object_count(&self, user_id: &str) -> usize {
        let data = self.data.lock().unwrap();
        data.get(user_id)
            .map(|u| u.values().map(|b| b.len()).sum())
            .unwrap_or(0)
    }
}

impl Default for MockMetadataStore {
    fn default() -> Self { Self::new() }
}

impl MetadataStorage for MockMetadataStore {
    fn put_metadata(&self, user_id: &str, bucket: &str, object_id: &str, metadata: &Metadata) -> Result<(), Error> {
        let mut data = self.data.lock().unwrap();
        let bucket_data = data
            .entry(user_id.to_string()).or_default()
            .entry(bucket.to_string()).or_default();

        if bucket_data.contains_key(object_id) {
            return Err(actix_web::error::ErrorBadRequest("Key already exists"));
        }
        bucket_data.insert(object_id.to_string(), metadata.clone());
        Ok(())
    }

    fn get_metadata(&self, user_id: &str, bucket: &str, object_id: &str) -> Result<Metadata, Error> {
        let data = self.data.lock().unwrap();
        data.get(user_id)
            .and_then(|u| u.get(bucket))
            .and_then(|b| b.get(object_id))
            .cloned()
            .ok_or_else(|| actix_web::error::ErrorNotFound(format!(
                "No data found for key: {} in bucket: {}, The key does not exist", object_id, bucket
            )))
    }

    fn delete_metadata(&self, user_id: &str, bucket: &str, object_id: &str) -> Result<(), Error> {
        let mut data = self.data.lock().unwrap();
        let removed = data
            .get_mut(user_id)
            .and_then(|u| u.get_mut(bucket))
            .and_then(|b| b.remove(object_id))
            .is_some();
        if removed {
            Ok(())
        } else {
            Err(actix_web::error::ErrorNotFound(format!(
                "No data found for key: {} in bucket: {}, The key does not exist", object_id, bucket
            )))
        }
    }

    fn list_objects(&self, user_id: &str, bucket: &str) -> Result<Vec<ObjectId>, Error> {
        let data = self.data.lock().unwrap();
        let mut keys: Vec<String> = data
            .get(user_id)
            .and_then(|u| u.get(bucket))
            .map(|b| b.keys().cloned().collect())
            .unwrap_or_default();
        keys.sort();
        Ok(keys)
    }

    fn object_exists(&self, user_id: &str, bucket: &str, object_id: &str) -> Result<bool, Error> {
        let data = self.data.lock().unwrap();
        Ok(data.get(user_id)
            .and_then(|u| u.get(bucket))
            .map(|b| b.contains_key(object_id))
            .unwrap_or(false))
    }

    fn update_metadata(&self, user_id: &str, bucket: &str, object_id: &str, metadata: &Metadata) -> Result<(), Error> {
        let mut data = self.data.lock().unwrap();
        let entry = data
            .get_mut(user_id)
            .and_then(|u| u.get_mut(bucket))
            .and_then(|b| b.get_mut(object_id));
        match entry {
            Some(e) => { *e = metadata.clone(); Ok(()) }
            None => Err(actix_web::error::ErrorNotFound(format!(
                "No data found for key: {}, The key does not exist", object_id
            ))),
        }
    }

    fn update_object_id(&self, user_id: &str, bucket: &str, old_object_id: &str, new_object_id: &str) -> Result<(), Error> {
        let mut data = self.data.lock().unwrap();
        let metadata = data
            .get_mut(user_id)
            .and_then(|u| u.get_mut(bucket))
            .and_then(|b| b.remove(old_object_id));
        match metadata {
            Some(m) => {
                data.get_mut(user_id).unwrap().get_mut(bucket).unwrap()
                    .insert(new_object_id.to_string(), m);
                Ok(())
            }
            None => Err(actix_web::error::ErrorNotFound(format!(
                "No data found for key: {}, The key does not exist", old_object_id
            ))),
        }
    }

    fn queue_deletion(&self, _user_id: &str, _bucket: &str, _key: &str, _offset_size_list: &[(u64, u64)]) -> Result<(), Error> {
        Ok(())
    }

    fn list_buckets_with_stats(&self, user_id: &str) -> Result<Vec<BucketStats>, Error> {
        let data = self.data.lock().unwrap();
        let buckets = self.buckets.lock().unwrap();

        // All registered buckets, including empty ones
        let registered = buckets.get(user_id).cloned().unwrap_or_default();
        let now = Utc::now().format("%Y-%m-%dT%H:%M:%S.000Z").to_string();
        let mut stats: Vec<BucketStats> = registered.iter().map(|name| {
            let (object_count, total_size) = data
                .get(user_id)
                .and_then(|u| u.get(name))
                .map(|b| {
                    let count = b.len() as u64;
                    let size: u64 = b.values().map(|m| m.size).sum();
                    (count, size)
                })
                .unwrap_or((0, 0));
            BucketStats { name: name.clone(), created_at: now.clone(), object_count, total_size }
        }).collect();
        stats.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(stats)
    }

    fn create_bucket(&self, user_id: &str, bucket: &str) -> Result<(), Error> {
        self.buckets.lock().unwrap()
            .entry(user_id.to_string()).or_default()
            .insert(bucket.to_string());
        Ok(())
    }

    fn delete_bucket(&self, user_id: &str, bucket: &str) -> Result<(), Error> {
        self.buckets.lock().unwrap()
            .entry(user_id.to_string()).or_default()
            .remove(bucket);
        Ok(())
    }

    fn bucket_exists(&self, user_id: &str, bucket: &str) -> Result<bool, Error> {
        Ok(self.buckets.lock().unwrap()
            .get(user_id)
            .map(|s| s.contains(bucket))
            .unwrap_or(false))
    }

    fn list_all_buckets_for_user(&self, user_id: &str) -> Result<Vec<String>, Error> {
        let buckets = self.buckets.lock().unwrap();
        let mut names: Vec<String> = buckets.get(user_id)
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default();
        names.sort();
        Ok(names)
    }

    fn bucket_object_stats(&self, user_id: &str, bucket: &str) -> Result<(u64, u64), Error> {
        let data = self.data.lock().unwrap();
        let (count, bytes) = data.get(user_id)
            .and_then(|u| u.get(bucket))
            .map(|b| {
                let count = b.len() as u64;
                let bytes: u64 = b.values().map(|m| m.size).sum();
                (count, bytes)
            })
            .unwrap_or((0, 0));
        Ok((count, bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mock_metadata_store_basic_operations() {
        let store = MockMetadataStore::new();
        let user_id = "test_user_mock";
        let object_id = "test_object_mock";

        store.create_bucket(user_id, "default").unwrap();

        let mut metadata = Metadata::from_offset_size_list(vec![(100, 200), (300, 400)]);
        metadata.properties.insert("test_prop".to_string(), "test_value".to_string());

        assert_eq!(store.user_count(), 0);
        store.put_metadata(user_id, "default", object_id, &metadata).unwrap();
        assert_eq!(store.object_count(user_id), 1);

        assert!(store.put_metadata(user_id, "default", object_id, &metadata).is_err());

        assert!(store.object_exists(user_id, "default", object_id).unwrap());
        assert!(!store.object_exists(user_id, "default", "nonexistent").unwrap());

        let retrieved = store.get_metadata(user_id, "default", object_id).unwrap();
        assert_eq!(retrieved.to_offset_size_list(), vec![(100, 200), (300, 400)]);
        assert_eq!(retrieved.properties.get("test_prop"), Some(&"test_value".to_string()));

        let objects = store.list_objects(user_id, "default").unwrap();
        assert!(objects.contains(&object_id.to_string()));

        let new_metadata = Metadata::from_offset_size_list(vec![(500, 600)]);
        store.update_metadata(user_id, "default", object_id, &new_metadata).unwrap();
        let updated = store.get_metadata(user_id, "default", object_id).unwrap();
        assert_eq!(updated.to_offset_size_list(), vec![(500, 600)]);

        let new_object_id = "new_test_object_mock";
        store.update_object_id(user_id, "default", object_id, new_object_id).unwrap();
        assert!(!store.object_exists(user_id, "default", object_id).unwrap());
        assert!(store.object_exists(user_id, "default", new_object_id).unwrap());

        store.delete_metadata(user_id, "default", new_object_id).unwrap();
        assert_eq!(store.object_count(user_id), 0);

        store.put_metadata(user_id, "default", object_id, &metadata).unwrap();
        store.clear();
        assert_eq!(store.user_count(), 0);
    }

    #[test]
    fn test_mock_bucket_lifecycle() {
        let store = MockMetadataStore::new();
        let user_id = "bucket_test_user";

        assert!(!store.bucket_exists(user_id, "my-bucket").unwrap());
        store.create_bucket(user_id, "my-bucket").unwrap();
        assert!(store.bucket_exists(user_id, "my-bucket").unwrap());

        let names = store.list_all_buckets_for_user(user_id).unwrap();
        assert!(names.contains(&"my-bucket".to_string()));

        store.delete_bucket(user_id, "my-bucket").unwrap();
        assert!(!store.bucket_exists(user_id, "my-bucket").unwrap());
    }
}
