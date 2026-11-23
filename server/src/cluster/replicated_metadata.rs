//! `MetadataStorage` for a cluster: S3 metadata kept as replicated rows.
//!
//! Every node holds a full copy of the rows, so reads are local. A write
//! changes one row: it reads the row, applies the change, stamps it with a
//! new `seq` (`hlc`) and replicates it to every node, returning once a
//! write quorum has it (`ordered`: every node keeps the newest stamp).
//!
//! **Rows**
//!
//! | Key | Holds |
//! |---|---|
//! | `bkt/{bucket}` | owner, creation time and every bucket setting |
//! | `ver/{user}/{bucket}/{key}\0{order}` | one object version; `order` is its creation stamp, so the last row of a key is the latest |
//! | `upl/{upload_id}` | a multipart upload and its parts |
//! | `acl/{bucket}/{key}/{version}` | an object version's ACL |
//! | `lck/{bucket}/{key}/{version}` | an object version's retention and legal hold |
//!
//! A write touches only the rows it changes: a versioned PUT appends one
//! version row, a tag change rewrites one, so a write's cost doesn't grow
//! with the key's history. (With all versions in one row, 1,000 versions
//! of a 4 KB key took 405 MB of log per node, and PUT p50 rose from 28 ms
//! to 44 ms.) Writes to one object key are serialized by a per-key lock on
//! this node. Under the single-write-node limitation, that serializes them
//! cluster-wide.
//!
//! **Versions.** A bucket that was never versioned keeps one version per
//! key, id `""`. With versioning enabled, every PUT appends a version with a
//! fresh id, and a DELETE without a version id appends a delete marker.
//! With versioning suspended, PUT and DELETE replace the `null` version. A
//! version written before versioning was enabled (id `""`) is the `null`
//! version from then on.

use actix_web::error::{ErrorBadRequest, ErrorInternalServerError, ErrorNotFound};
use actix_web::Error;
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

use warp_meta_store::hlc;
use warp_meta_store::{Applied, Entry, Record, WarpMetaStore};
use crate::metadata::{
    BucketStats, DeleteSpecificResult, DeletionEvent, Metadata, MetadataStorage, MultipartPartRow, MultipartUploadRow,
    ObjectId, ObjectLockRow, VersionRow, VersioningDeleteResult,
};
use super::blob_store::{blob_at, index_blob, BlobRef};
use super::meta_engine::{AnyEngine, MetaEngineKind};
use warp_meta_store::Engine;

// ---------------------------------------------------------------- rows

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BucketRow {
    pub owner: String,
    pub created_at: String,
    /// "disabled" (never versioned), "enabled" or "suspended", as the
    /// handlers write it.
    pub versioning: String,
    pub location: String,
    pub lock_enabled: bool,
    pub lock_config: Option<(String, Option<i64>, Option<i64>)>,
    pub cors: Option<String>,
    pub tags: Vec<(String, String)>,
    pub acl: Option<(String, String)>,
    pub public_access_block: Option<(bool, bool, bool, bool)>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VersionEntry {
    /// Creation stamp (`hlc`): the version's place in its key's history
    /// and the last part of its row key.
    #[serde(default)]
    pub order: u64,
    /// `""` for a bucket that was never versioned, `"null"` for the null
    /// version of a suspended bucket, otherwise a generated id.
    pub version_id: String,
    pub is_delete_marker: bool,
    pub meta: Metadata,
    pub tags: Vec<(String, String)>,
    pub parts_manifest: Option<String>,
    /// Where this version's bytes are: one entry per stored range. The row
    /// is the only record of a blob's placement.
    #[serde(default)]
    pub blobs: Vec<BlobRef>,
}

impl VersionEntry {
    fn is_null(&self) -> bool {
        self.version_id.is_empty() || self.version_id == "null"
    }

    /// The id as S3 shows it.
    fn shown_id(&self) -> String {
        if self.version_id.is_empty() { "null".to_string() } else { self.version_id.clone() }
    }
}

/// All of one key's versions, oldest first: an in-memory view assembled
/// from its version rows.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ObjectRow {
    pub versions: Vec<VersionEntry>,
}

impl ObjectRow {
    fn latest(&self) -> Option<&VersionEntry> {
        self.versions.last()
    }

    fn find(&self, version_id: &str) -> Option<usize> {
        let null = version_id.is_empty() || version_id == "null";
        self.versions.iter().position(|v| if null { v.is_null() } else { v.version_id == version_id })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PartRow {
    pub etag: String,
    pub size: u64,
    pub extents_blob: Vec<u8>,
    pub checksum_value: String,
    /// Where the part's bytes are (see `VersionEntry::blobs`).
    #[serde(default)]
    pub blobs: Vec<BlobRef>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UploadRow {
    pub user_id: String,
    pub bucket: String,
    pub key: String,
    pub content_type: Option<String>,
    pub metadata_json: String,
    pub initiated_at: String,
    pub status: String,
    pub final_etag: Option<String>,
    pub checksum_algorithm: String,
    pub checksum_type: String,
    pub object_lock_mode: String,
    pub object_lock_retain_until: String,
    pub object_lock_legal_hold: String,
    pub tagging: String,
    pub parts: BTreeMap<i32, PartRow>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LockRow {
    pub mode: Option<String>,
    pub retain_until_date: Option<String>,
    pub legal_hold: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MetaValue {
    Bucket(BucketRow),
    Version(VersionEntry),
    Upload(UploadRow),
    Acl { owner: String, grants: String },
    Lock(LockRow),
    /// Blobs a write just stopped referencing (RFC 6 step 6). Written after
    /// the rows that dropped them, so a crash in between leaks space and
    /// never frees a referenced blob. Each node frees its own shards of
    /// them once the grace period has passed (`gc.rs`).
    Gc(Vec<BlobRef>),
}

/// One replicated row, with its place in the row's write order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetaRecord {
    pub seq: u64,
    pub value: MetaValue,
}

impl Record for MetaRecord {
    fn seq(&self) -> u64 {
        self.seq
    }
    fn version(&self) -> &str {
        ""
    }
}

/// This node's copy of the rows, on the engine picked at startup
/// (`WARPDRIVE_META_ENGINE`, see `meta_engine`).
pub type MetaRows = WarpMetaStore<MetaRecord, AnyEngine<Entry<MetaRecord>>>;

pub fn open_rows(kind: MetaEngineKind, path: &str) -> std::io::Result<MetaRows> {
    let rows: MetaRows = WarpMetaStore::new(AnyEngine::open(kind, path, "s3 metadata")?);
    for (_, entry) in rows.engine().snapshot() {
        if let Some(r) = entry.live() {
            index_record(r);
        }
    }
    Ok(rows)
}

/// Adds every blob a row references to this node's address index, so this
/// node can read them. Called for each row it applies, and at startup.
pub fn index_record(r: &MetaRecord) {
    match &r.value {
        MetaValue::Version(v) => v.blobs.iter().for_each(index_blob),
        MetaValue::Upload(u) => u.parts.values().flat_map(|p| &p.blobs).for_each(index_blob),
        _ => {}
    }
}

/// The placements of the blobs starting at `extents`' offsets, as this
/// node's index knows them (it wrote them, or applied their rows).
fn blobs_for(user: &str, bucket: &str, extents: &[(u64, u64)]) -> Vec<BlobRef> {
    extents.iter().filter_map(|(offset, _)| blob_at(user, bucket, *offset)).collect()
}

fn bucket_key(bucket: &str) -> String {
    format!("bkt/{bucket}")
}
fn bucket_versions_prefix(user: &str, bucket: &str) -> String {
    format!("ver/{}:{user}/{}:{bucket}/", user.len(), bucket.len())
}
/// Every version row of one key starts with this. Object keys can't
/// contain NUL, and NUL sorts first, so key order is preserved.
fn versions_prefix(user: &str, bucket: &str, key: &str) -> String {
    format!("{}{key}\0", bucket_versions_prefix(user, bucket))
}
fn version_key(user: &str, bucket: &str, key: &str, order: u64) -> String {
    format!("{}{order:016x}", versions_prefix(user, bucket, key))
}
/// After every version row of a key (hex digits sort below 'g').
fn versions_end(user: &str, bucket: &str, key: &str) -> String {
    format!("{}g", versions_prefix(user, bucket, key))
}
fn upload_key(upload_id: &str) -> String {
    format!("upl/{upload_id}")
}
fn version_scoped(kind: &str, bucket: &str, key: &str, version_id: &str) -> String {
    let vid = if version_id == "null" { "" } else { version_id };
    format!("{kind}/{}:{bucket}/{}:{key}/{vid}", bucket.len(), key.len())
}

// ---------------------------------------------------------------- replication

/// Where a row write goes. Production replicates to a quorum of nodes;
/// tests apply locally.
pub trait RowWriter: Send + Sync {
    /// Writes `record` (or a tombstone at `seq` when `None`) for `key`,
    /// returning once it is durable on a write quorum.
    fn write(&self, key: &str, seq: u64, record: Option<MetaRecord>) -> Result<(), String>;

    /// Async form of `write`, for callers that must not block a thread on
    /// the quorum round trip. The default runs `write` now.
    fn write_async(&self, key: String, seq: u64, record: Option<MetaRecord>) -> BoxFuture<'static, Result<(), String>> {
        Box::pin(std::future::ready(self.write(&key, seq, record)))
    }
}

static ROWS: std::sync::OnceLock<Arc<MetaRows>> = std::sync::OnceLock::new();

/// Sets this node's rows, so replicated row writes from peers land in them.
/// Called once at startup.
pub fn install_rows(rows: Arc<MetaRows>) {
    let _ = ROWS.set(rows);
}

/// This node's rows, once `install_rows` has run.
pub fn rows() -> Option<Arc<MetaRows>> {
    ROWS.get().cloned()
}

/// Replicates row writes to a write quorum of the cluster.
pub struct ClusterWriter;

impl RowWriter for ClusterWriter {
    fn write(&self, key: &str, seq: u64, record: Option<MetaRecord>) -> Result<(), String> {
        let state = super::bridge::state().ok_or("cluster state not installed")?;
        let peers = state.membership.peers();
        let required = super::coordinator::required_write_acks(state.ec.k(), state.ec.m());
        let key = key.to_string();
        let acked = super::bridge::run(move || async move { super::coordinator::replicate_meta_row(&state, &peers, &key, seq, record).await });
        if acked < required {
            return Err(format!("{acked}/{required} nodes stored the row"));
        }
        Ok(())
    }

    fn write_async(&self, key: String, seq: u64, record: Option<MetaRecord>) -> BoxFuture<'static, Result<(), String>> {
        Box::pin(async move {
            let state = super::bridge::state().ok_or("cluster state not installed")?;
            let peers = state.membership.peers();
            let required = super::coordinator::required_write_acks(state.ec.k(), state.ec.m());
            let acked = super::coordinator::replicate_meta_row(&state, &peers, &key, seq, record).await;
            if acked < required {
                return Err(format!("{acked}/{required} nodes stored the row"));
            }
            Ok(())
        })
    }
}

/// Applies row writes to this node's rows only.
pub struct LocalWriter(pub Arc<MetaRows>);

impl RowWriter for LocalWriter {
    fn write(&self, key: &str, seq: u64, record: Option<MetaRecord>) -> Result<(), String> {
        let res = match record {
            Some(r) => {
                index_record(&r);
                self.0.put(key.to_string(), r)
            }
            None => self.0.delete(key.to_string(), seq),
        }
        .map_err(|e| e.to_string())?;
        match res {
            Applied::Yes { .. } => Ok(()),
            other => Err(format!("row write refused: {other:?}")),
        }
    }
}

// ---------------------------------------------------------------- store

/// Striped per-key locks: writes to one row are serialized on this node.
///
/// The stripes are async-aware mutexes so the async PUT path can hold one
/// across a replication round trip without blocking a thread. Sync callers
/// block their thread on the same mutex, as they did on the old blocking
/// one, so both paths exclude each other. An async holder runs on the
/// bridge runtime, never on a thread a sync caller is blocking.
struct KeyLocks(Vec<tokio::sync::Mutex<()>>);

impl KeyLocks {
    fn new() -> Self {
        Self((0..256).map(|_| tokio::sync::Mutex::new(())).collect())
    }
    fn stripe(&self, key: &str) -> &tokio::sync::Mutex<()> {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        key.hash(&mut h);
        &self.0[(h.finish() as usize) % self.0.len()]
    }
    fn lock(&self, key: &str) -> tokio::sync::MutexGuard<'_, ()> {
        futures::executor::block_on(self.stripe(key).lock())
    }
    async fn lock_async(&self, key: &str) -> tokio::sync::MutexGuard<'_, ()> {
        self.stripe(key).lock().await
    }
}

pub struct ReplicatedMetadataStore {
    rows: Arc<MetaRows>,
    writer: Box<dyn RowWriter>,
    locks: KeyLocks,
}

fn now_iso() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S.000Z").to_string()
}

fn not_found(bucket: &str, key: &str) -> Error {
    ErrorNotFound(format!("No data found for key: {key} in bucket: {bucket}, The key does not exist"))
}

fn write_err(e: String) -> Error {
    ErrorInternalServerError(format!("metadata write did not reach quorum: {e}"))
}

/// A new version id: increasing in write order, so ids sort newest-last.
fn new_version_id() -> String {
    format!("{:016x}", hlc::now())
}

impl ReplicatedMetadataStore {
    pub fn new(rows: Arc<MetaRows>, writer: Box<dyn RowWriter>) -> Self {
        Self { rows, writer, locks: KeyLocks::new() }
    }

    fn get(&self, key: &str) -> Option<MetaValue> {
        self.rows.get(key).map(|r| r.value)
    }

    fn put(&self, key: &str, value: MetaValue) -> Result<(), Error> {
        let seq = hlc::now();
        self.writer.write(key, seq, Some(MetaRecord { seq, value })).map_err(write_err)
    }

    fn remove(&self, key: &str) -> Result<(), Error> {
        if self.rows.get(key).is_none() {
            return Ok(());
        }
        self.writer.write(key, hlc::now(), None).map_err(write_err)
    }

    fn bucket(&self, bucket: &str) -> Option<BucketRow> {
        match self.get(&bucket_key(bucket)) {
            Some(MetaValue::Bucket(b)) => Some(b),
            _ => None,
        }
    }

    /// Read-modify-write of a bucket row. `f` returns what to hand back.
    fn update_bucket<T>(&self, bucket: &str, f: impl FnOnce(&mut BucketRow) -> T) -> Result<T, Error> {
        let key = bucket_key(bucket);
        let _g = self.locks.lock(&key);
        let mut row = self.bucket(bucket).ok_or_else(|| ErrorNotFound(format!("bucket {bucket} does not exist")))?;
        let out = f(&mut row);
        self.put(&key, MetaValue::Bucket(row))?;
        Ok(out)
    }

    /// Every version of a key, oldest first. `None` if it has none.
    fn object(&self, user: &str, bucket: &str, key: &str) -> Option<ObjectRow> {
        let versions: Vec<VersionEntry> = self
            .rows
            .list_live(&versions_prefix(user, bucket, key), |_, r| match &r.value {
                MetaValue::Version(v) => Some(v.clone()),
                _ => None,
            })
            .into_iter()
            .flatten()
            .collect();
        (!versions.is_empty()).then_some(ObjectRow { versions })
    }

    /// The latest version of a key, found with one lookup.
    fn latest(&self, user: &str, bucket: &str, key: &str) -> Option<VersionEntry> {
        match self.rows.floor_live(&versions_prefix(user, bucket, key), &versions_end(user, bucket, key)) {
            Some((_, MetaRecord { value: MetaValue::Version(v), .. })) => Some(v),
            _ => None,
        }
    }

    /// Read-modify-write of a key's versions. Writes only the version rows
    /// `f` added or changed, and tombstones the ones it removed.
    fn update_object<T>(&self, user: &str, bucket: &str, key: &str, f: impl FnOnce(&mut ObjectRow) -> Result<T, Error>) -> Result<T, Error> {
        let _g = self.locks.lock(&versions_prefix(user, bucket, key));
        let before = self.object(user, bucket, key).unwrap_or_default();
        let mut after = before.clone();
        let out = f(&mut after)?;
        for (row, value) in version_writes(user, bucket, key, &before, &after) {
            match value {
                Some(v) => self.put(&row, v)?,
                None => self.remove(&row)?,
            }
        }
        Ok(out)
    }

    async fn put_async(&self, key: String, value: MetaValue) -> Result<(), String> {
        let seq = hlc::now();
        self.writer.write_async(key, seq, Some(MetaRecord { seq, value })).await.map_err(|e| format!("metadata write did not reach quorum: {e}"))
    }

    async fn remove_async(&self, key: String) -> Result<(), String> {
        if self.rows.get(&key).is_none() {
            return Ok(());
        }
        self.writer.write_async(key, hlc::now(), None).await.map_err(|e| format!("metadata write did not reach quorum: {e}"))
    }

    /// The S3 PUT metadata write without blocking a thread: the same rows,
    /// lock and order as `put_object_v2`, awaited instead of bridged.
    async fn put_object_v2_owned(self: Arc<Self>, user: String, bucket: String, key: String, metadata: Metadata) -> Result<(Option<String>, Vec<(u64, u64)>), String> {
        let mode = self.mode(&bucket);
        let (vid, replaced) = {
            let _g = self.locks.lock_async(&versions_prefix(&user, &bucket, &key)).await;
            let before = self.object(&user, &bucket, &key).unwrap_or_default();
            let mut after = before.clone();
            let out = put_version(mode, &user, &bucket, &metadata, &mut after);
            for (row, value) in version_writes(&user, &bucket, &key, &before, &after) {
                match value {
                    Some(v) => self.put_async(row, v).await?,
                    None => self.remove_async(row).await?,
                }
            }
            out
        };
        let old_extents = match replaced {
            Some(v) => {
                self.forget_version_async(&bucket, &key, &v).await?;
                v.meta.to_offset_size_list()
            }
            None => Vec::new(),
        };
        Ok(((!vid.is_empty()).then_some(vid), old_extents))
    }

    /// `update_object` awaited instead of bridged: the same lock, read and
    /// row writes. Runs on the cluster runtime, never on an HTTP worker.
    async fn update_object_async<T>(&self, user: &str, bucket: &str, key: &str, f: impl FnOnce(&mut ObjectRow) -> T) -> Result<T, String> {
        let _g = self.locks.lock_async(&versions_prefix(user, bucket, key)).await;
        let before = self.object(user, bucket, key).unwrap_or_default();
        let mut after = before.clone();
        let out = f(&mut after);
        for (row, value) in version_writes(user, bucket, key, &before, &after) {
            match value {
                Some(v) => self.put_async(row, v).await?,
                None => self.remove_async(row).await?,
            }
        }
        Ok(out)
    }

    /// A version is gone: its ACL and lock rows go, and its blobs are dead.
    async fn forget_version_async(&self, bucket: &str, key: &str, v: &VersionEntry) -> Result<(), String> {
        self.remove_async(version_scoped("acl", bucket, key, &v.version_id)).await?;
        self.remove_async(version_scoped("lck", bucket, key, &v.version_id)).await?;
        self.gc_async(v.blobs.clone()).await
    }

    /// Records `blobs` as dead (a `gc/` row), after whatever dropped them.
    async fn gc_async(&self, blobs: Vec<BlobRef>) -> Result<(), String> {
        let blobs = super::gc::with_shards(blobs);
        if blobs.is_empty() {
            return Ok(());
        }
        self.put_async(super::gc::new_key(), MetaValue::Gc(blobs)).await
    }

    /// The whole S3 DeleteObject metadata write without blocking a thread:
    /// a delete marker under versioning, else the object's removal. The
    /// same rows as `delete_object_v2` followed by `delete_metadata`.
    async fn delete_object_owned(self: Arc<Self>, user: String, bucket: String, key: String) -> Result<VersioningDeleteResult, String> {
        let mode = self.mode(&bucket);
        if mode == Versioning::Disabled {
            if let Some(v) = self.update_object_async(&user, &bucket, &key, |o| o.versions.pop()).await? {
                self.forget_version_async(&bucket, &key, &v).await?;
            }
            return Ok(VersioningDeleteResult::Deleted);
        }
        let (vid, replaced) = self
            .update_object_async(&user, &bucket, &key, |o| {
                let (vid, replaced) = if mode == Versioning::Enabled {
                    (new_version_id(), None)
                } else {
                    ("null".to_string(), o.find("null").map(|i| o.versions.remove(i)))
                };
                let mut m = Metadata::from_offset_size_list(Vec::new());
                m.is_delete_marker = true;
                m.version_id = Some(vid.clone());
                m.last_modified = Some(now_iso());
                o.versions.push(VersionEntry { order: hlc::now(), version_id: vid.clone(), is_delete_marker: true, meta: m, tags: Vec::new(), parts_manifest: None, blobs: Vec::new() });
                (vid, replaced)
            })
            .await?;
        if let Some(v) = replaced {
            self.forget_version_async(&bucket, &key, &v).await?;
        }
        Ok(VersioningDeleteResult::Marker { version_id: vid })
    }

    /// `update_upload` awaited: the same lock, read and row write.
    async fn update_upload_async(&self, upload_id: &str, f: impl FnOnce(&mut UploadRow)) -> Result<(), (u16, String)> {
        let key = upload_key(upload_id);
        let _g = self.locks.lock_async(&key).await;
        let Some(mut row) = self.upload(upload_id) else {
            return Err((404, format!("no multipart upload {upload_id}")));
        };
        f(&mut row);
        self.put_async(key, MetaValue::Upload(row)).await.map_err(|e| (500, e))
    }

    fn upload(&self, upload_id: &str) -> Option<UploadRow> {
        match self.get(&upload_key(upload_id)) {
            Some(MetaValue::Upload(u)) => Some(u),
            _ => None,
        }
    }

    fn update_upload(&self, upload_id: &str, f: impl FnOnce(&mut UploadRow)) -> Result<(), Error> {
        let key = upload_key(upload_id);
        let _g = self.locks.lock(&key);
        let Some(mut row) = self.upload(upload_id) else {
            return Err(ErrorNotFound(format!("no multipart upload {upload_id}")));
        };
        f(&mut row);
        self.put(&key, MetaValue::Upload(row))
    }

    fn versioning(&self, bucket: &str) -> String {
        self.bucket(bucket).map(|b| b.versioning).unwrap_or_else(|| "disabled".to_string())
    }

    /// The bucket's versioning mode, whatever case it was stored in.
    fn mode(&self, bucket: &str) -> Versioning {
        match self.versioning(bucket).to_ascii_lowercase().as_str() {
            "enabled" => Versioning::Enabled,
            "suspended" => Versioning::Suspended,
            _ => Versioning::Disabled,
        }
    }

    /// Every key in a bucket with its versions, in key order.
    fn objects_in(&self, user: &str, bucket: &str) -> Vec<(String, ObjectRow)> {
        let prefix = bucket_versions_prefix(user, bucket);
        let mut out: Vec<(String, ObjectRow)> = Vec::new();
        let rows = self.rows.list_live(&prefix, |k, r| match &r.value {
            MetaValue::Version(v) => k[prefix.len()..].rsplit_once('\0').map(|(key, _)| (key.to_string(), v.clone())),
            _ => None,
        });
        for (key, v) in rows.into_iter().flatten() {
            match out.last_mut() {
                Some((k, o)) if *k == key => o.versions.push(v),
                _ => out.push((key, ObjectRow { versions: vec![v] })),
            }
        }
        out
    }

    fn lock_row(&self, bucket: &str, key: &str, version_id: &str) -> Option<LockRow> {
        match self.get(&version_scoped("lck", bucket, key, version_id)) {
            Some(MetaValue::Lock(l)) => Some(l),
            _ => None,
        }
    }

    /// Drops the ACL and lock rows of a removed version.
    /// A version is gone: its ACL and lock rows go, and its blobs are dead.
    fn forget_version(&self, bucket: &str, key: &str, v: &VersionEntry) -> Result<(), Error> {
        self.remove(&version_scoped("acl", bucket, key, &v.version_id))?;
        self.remove(&version_scoped("lck", bucket, key, &v.version_id))?;
        self.gc(v.blobs.clone())
    }

    /// Records `blobs` as dead (a `gc/` row), after whatever dropped them.
    fn gc(&self, blobs: Vec<BlobRef>) -> Result<(), Error> {
        let blobs = super::gc::with_shards(blobs);
        if blobs.is_empty() {
            return Ok(());
        }
        self.put(&super::gc::new_key(), MetaValue::Gc(blobs))
    }
}

/// Adds a new version of an object for an S3 PUT under `mode`. Returns
/// the version id and the version it replaced, if any.
fn put_version(mode: Versioning, user_id: &str, bucket: &str, metadata: &Metadata, o: &mut ObjectRow) -> (String, Option<VersionEntry>) {
    let mut meta = metadata.clone();
    meta.is_delete_marker = false;
    let (vid, replaced) = match mode {
        Versioning::Enabled => (new_version_id(), None),
        Versioning::Suspended => {
            let old = o.find("null").map(|i| o.versions.remove(i));
            ("null".to_string(), old)
        }
        _ => (String::new(), o.versions.drain(..).last()),
    };
    meta.version_id = (!vid.is_empty()).then(|| vid.clone());
    let blobs = blobs_for(user_id, bucket, &meta.to_offset_size_list());
    // An unversioned overwrite takes the old version's slot: one row
    // write, not a new row plus a tombstone.
    let order = match (mode, &replaced) {
        (Versioning::Disabled, Some(old)) => old.order,
        _ => hlc::now(),
    };
    o.versions.push(VersionEntry { order, version_id: vid.clone(), is_delete_marker: false, meta, tags: Vec::new(), parts_manifest: None, blobs });
    (vid, replaced)
}

/// The row writes that turn `before` into `after`: new or changed version
/// rows first, then tombstones for removed ones.
fn version_writes(user: &str, bucket: &str, key: &str, before: &ObjectRow, after: &ObjectRow) -> Vec<(String, Option<MetaValue>)> {
    let old: BTreeMap<u64, &VersionEntry> = before.versions.iter().map(|v| (v.order, v)).collect();
    let new: BTreeMap<u64, &VersionEntry> = after.versions.iter().map(|v| (v.order, v)).collect();
    let mut writes = Vec::new();
    for (order, v) in &new {
        if old.get(order) != Some(v) {
            writes.push((version_key(user, bucket, key, *order), Some(MetaValue::Version((*v).clone()))));
        }
    }
    for order in old.keys().filter(|o| !new.contains_key(o)) {
        writes.push((version_key(user, bucket, key, *order), None));
    }
    writes
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Versioning {
    Disabled,
    Enabled,
    Suspended,
}

/// Visible size and count of a bucket's latest, non-deleted objects.
fn stats(objects: &[(String, ObjectRow)]) -> (u64, u64) {
    objects
        .iter()
        .filter_map(|(_, o)| o.latest().filter(|v| !v.is_delete_marker))
        .fold((0, 0), |(n, b), v| (n + 1, b + v.meta.size))
}

impl MetadataStorage for ReplicatedMetadataStore {
    fn put_object_v2_async(self: Arc<Self>, user_id: String, bucket: String, key: String, metadata: Metadata) -> Result<BoxFuture<'static, Result<(Option<String>, Vec<(u64, u64)>), String>>, Metadata> {
        Ok(Box::pin(self.put_object_v2_owned(user_id, bucket, key, metadata)))
    }

    fn upsert_multipart_part_async(
        self: Arc<Self>, upload_id: String, part_number: i32, etag: String, size: u64, extents_blob: Vec<u8>, checksum_value: String,
    ) -> Option<BoxFuture<'static, Result<(), (u16, String)>>> {
        Some(Box::pin(async move {
            let extents = crate::util::serializer::deserialize_offset_size(&extents_blob).map_err(|e| (400, e.to_string()))?;
            let mut replaced = Vec::new();
            self.update_upload_async(&upload_id, |u| {
                let blobs = blobs_for(&u.user_id, &u.bucket, &extents);
                if let Some(old) = u.parts.insert(part_number, PartRow { etag, size, extents_blob, checksum_value, blobs }) {
                    replaced = old.blobs;
                }
            })
            .await?;
            self.gc_async(replaced).await.map_err(|e| (500, e))
        }))
    }

    fn delete_specific_version_async(self: Arc<Self>, user_id: String, bucket: String, key: String, version_id: String) -> Option<BoxFuture<'static, Result<DeleteSpecificResult, String>>> {
        Some(Box::pin(async move {
            let vid = version_id.clone();
            let removed = self.update_object_async(&user_id, &bucket, &key, move |o| o.find(&vid).map(|i| o.versions.remove(i))).await?;
            match removed {
                Some(v) => {
                    self.forget_version_async(&bucket, &key, &v).await?;
                    Ok(DeleteSpecificResult { found: true, was_delete_marker: v.is_delete_marker, version_id: v.shown_id() })
                }
                None => Ok(DeleteSpecificResult { found: false, was_delete_marker: false, version_id }),
            }
        }))
    }

    fn delete_object_async(self: Arc<Self>, user_id: String, bucket: String, key: String) -> Option<BoxFuture<'static, Result<VersioningDeleteResult, String>>> {
        Some(Box::pin(self.delete_object_owned(user_id, bucket, key)))
    }

    // ---- objects (legacy native-API path) ----

    fn put_metadata(&self, user_id: &str, bucket: &str, object_id: &str, metadata: &Metadata) -> Result<(), Error> {
        self.update_object(user_id, bucket, object_id, |o| {
            if o.latest().is_some_and(|v| !v.is_delete_marker) {
                return Err(ErrorBadRequest("Key already exists"));
            }
            let blobs = blobs_for(user_id, bucket, &metadata.to_offset_size_list());
            o.versions = vec![VersionEntry { order: hlc::now(), version_id: String::new(), is_delete_marker: false, meta: metadata.clone(), tags: Vec::new(), parts_manifest: None, blobs }];
            Ok(())
        })
    }

    fn get_metadata(&self, user_id: &str, bucket: &str, object_id: &str) -> Result<Metadata, Error> {
        self.latest(user_id, bucket, object_id)
            .filter(|v| !v.is_delete_marker)
            .map(|v| v.meta)
            .ok_or_else(|| not_found(bucket, object_id))
    }

    /// Removes the latest version (all of a never-versioned key).
    fn delete_metadata(&self, user_id: &str, bucket: &str, object_id: &str) -> Result<(), Error> {
        let removed = self.update_object(user_id, bucket, object_id, |o| Ok(o.versions.pop()))?;
        match removed {
            Some(v) => self.forget_version(bucket, object_id, &v),
            None => Err(not_found(bucket, object_id)),
        }
    }

    fn list_objects(&self, user_id: &str, bucket: &str) -> Result<Vec<ObjectId>, Error> {
        self.list_objects_with_prefix(user_id, bucket, "")
    }

    /// A range scan over the prefix's version rows that carries only each
    /// key and whether its latest version is a delete marker.
    fn list_objects_with_prefix(&self, user_id: &str, bucket: &str, prefix: &str) -> Result<Vec<ObjectId>, Error> {
        let base = bucket_versions_prefix(user_id, bucket);
        let rows = self.rows.list_live(&format!("{base}{prefix}"), |k, r| match &r.value {
            MetaValue::Version(v) => k[base.len()..].rsplit_once('\0').map(|(key, _)| (key.to_string(), v.is_delete_marker)),
            _ => None,
        });
        // Rows come in key order, oldest version first: a key's last row is
        // its latest version.
        let mut out: Vec<ObjectId> = Vec::new();
        let mut current: Option<(String, bool)> = None;
        for (key, marker) in rows.into_iter().flatten() {
            if let Some((k, m)) = current.take() {
                if k != key && !m {
                    out.push(k);
                }
            }
            current = Some((key, marker));
        }
        if let Some((k, false)) = current {
            out.push(k);
        }
        Ok(out)
    }

    fn object_exists(&self, user_id: &str, bucket: &str, object_id: &str) -> Result<bool, Error> {
        Ok(self.latest(user_id, bucket, object_id).is_some_and(|v| !v.is_delete_marker))
    }

    fn update_metadata(&self, user_id: &str, bucket: &str, object_id: &str, metadata: &Metadata) -> Result<(), Error> {
        self.update_object(user_id, bucket, object_id, |o| match o.versions.last_mut() {
            Some(v) if !v.is_delete_marker => {
                v.meta = metadata.clone();
                v.blobs = blobs_for(user_id, bucket, &metadata.to_offset_size_list());
                Ok(())
            }
            _ => Err(not_found(bucket, object_id)),
        })
    }

    fn update_object_id(&self, user_id: &str, bucket: &str, old_object_id: &str, new_object_id: &str) -> Result<(), Error> {
        let row = self.object(user_id, bucket, old_object_id).ok_or_else(|| not_found(bucket, old_object_id))?;
        self.update_object(user_id, bucket, new_object_id, |o| {
            *o = row;
            Ok(())
        })?;
        self.update_object(user_id, bucket, old_object_id, |o| {
            o.versions.clear();
            Ok(())
        })
    }

    /// Space reclamation comes with RFC 6. Until then, unreferenced ranges
    /// are kept.
    fn queue_deletion(&self, _user_id: &str, _bucket: &str, _key: &str, _offset_size_list: &[(u64, u64)]) -> Result<(), Error> {
        Ok(())
    }

    // ---- buckets ----

    fn list_buckets_with_stats(&self, user_id: &str) -> Result<Vec<BucketStats>, Error> {
        let mut out = Vec::new();
        for name in self.list_all_buckets_for_user(user_id)? {
            let created_at = self.bucket(&name).map(|b| b.created_at).unwrap_or_default();
            let (object_count, total_size) = stats(&self.objects_in(user_id, &name));
            out.push(BucketStats { name, created_at, object_count, total_size });
        }
        Ok(out)
    }

    fn create_bucket(&self, user_id: &str, bucket: &str) -> Result<(), Error> {
        self.create_bucket_with_lock(user_id, bucket, false)
    }

    fn delete_bucket(&self, user_id: &str, bucket: &str) -> Result<(), Error> {
        let key = bucket_key(bucket);
        let _g = self.locks.lock(&key);
        match self.bucket(bucket) {
            Some(b) if b.owner == user_id => self.remove(&key),
            _ => Ok(()),
        }
    }

    fn bucket_exists(&self, user_id: &str, bucket: &str) -> Result<bool, Error> {
        Ok(self.bucket(bucket).is_some_and(|b| b.owner == user_id))
    }

    fn list_all_buckets_for_user(&self, user_id: &str) -> Result<Vec<String>, Error> {
        Ok(self
            .rows
            .list_live("bkt/", |k, r| match &r.value {
                MetaValue::Bucket(b) if b.owner == user_id => Some(k["bkt/".len()..].to_string()),
                _ => None,
            })
            .into_iter()
            .flatten()
            .collect())
    }

    fn bucket_object_stats(&self, user_id: &str, bucket: &str) -> Result<(u64, u64), Error> {
        Ok(stats(&self.objects_in(user_id, bucket)))
    }

    // ---- S3 PUT path ----

    fn put_object_v2(&self, user_id: &str, bucket: &str, key: &str, metadata: &Metadata) -> Result<(Option<String>, Vec<(u64, u64)>), Error> {
        let mode = self.mode(bucket);
        let (vid, replaced) = self.update_object(user_id, bucket, key, |o| Ok(put_version(mode, user_id, bucket, metadata, o)))?;
        let old_extents = match replaced {
            Some(v) => {
                self.forget_version(bucket, key, &v)?;
                v.meta.to_offset_size_list()
            }
            None => Vec::new(),
        };
        Ok(((!vid.is_empty()).then_some(vid), old_extents))
    }

    // ---- deletion WAL: reclamation comes with RFC 6 ----

    fn get_pending_deletions(&self, _limit: i32) -> Result<Vec<DeletionEvent>, Error> {
        Ok(Vec::new())
    }
    fn mark_deletion_processed(&self, _id: i64) -> Result<(), Error> {
        Ok(())
    }
    fn cleanup_old_deletions(&self) -> Result<usize, Error> {
        Ok(0)
    }

    // ---- CORS ----

    fn set_bucket_cors(&self, bucket: &str, cors_xml: &str) -> Result<(), Error> {
        self.update_bucket(bucket, |b| b.cors = Some(cors_xml.to_string()))
    }
    fn get_bucket_cors(&self, bucket: &str) -> Result<Option<String>, Error> {
        Ok(self.bucket(bucket).and_then(|b| b.cors))
    }
    fn delete_bucket_cors(&self, bucket: &str) -> Result<(), Error> {
        if self.bucket(bucket).is_none() {
            return Ok(());
        }
        self.update_bucket(bucket, |b| b.cors = None)
    }

    // ---- location ----

    fn set_bucket_location(&self, _user_id: &str, bucket: &str, location: &str) -> Result<(), Error> {
        self.update_bucket(bucket, |b| b.location = location.to_string())
    }
    fn get_bucket_location(&self, _user_id: &str, bucket: &str) -> Result<String, Error> {
        Ok(self.bucket(bucket).map(|b| b.location).unwrap_or_default())
    }

    // ---- versioning ----

    fn get_versioning_state(&self, bucket: &str) -> Result<String, Error> {
        Ok(self.versioning(bucket))
    }

    fn set_versioning_state(&self, _user_id: &str, bucket: &str, state: &str) -> Result<(), Error> {
        self.update_bucket(bucket, |b| b.versioning = state.to_string())
    }

    fn delete_object_v2(&self, user_id: &str, bucket: &str, key: &str) -> Result<VersioningDeleteResult, Error> {
        let mode = self.mode(bucket);
        if mode == Versioning::Disabled {
            // Not versioned: the caller removes the object.
            return Ok(VersioningDeleteResult::Deleted);
        }
        let marker_meta = |vid: &str| {
            let mut m = Metadata::from_offset_size_list(Vec::new());
            m.is_delete_marker = true;
            m.version_id = Some(vid.to_string());
            m.last_modified = Some(now_iso());
            m
        };
        let (vid, replaced) = self.update_object(user_id, bucket, key, |o| {
            let (vid, replaced) = if mode == Versioning::Enabled {
                (new_version_id(), None)
            } else {
                ("null".to_string(), o.find("null").map(|i| o.versions.remove(i)))
            };
            o.versions.push(VersionEntry { order: hlc::now(), version_id: vid.clone(), is_delete_marker: true, meta: marker_meta(&vid), tags: Vec::new(), parts_manifest: None, blobs: Vec::new() });
            Ok((vid, replaced))
        })?;
        if let Some(v) = replaced {
            self.forget_version(bucket, key, &v)?;
        }
        Ok(VersioningDeleteResult::Marker { version_id: vid })
    }

    fn delete_specific_version(&self, user_id: &str, bucket: &str, key: &str, version_id: &str) -> Result<DeleteSpecificResult, Error> {
        let removed = self.update_object(user_id, bucket, key, |o| Ok(o.find(version_id).map(|i| o.versions.remove(i))))?;
        match removed {
            Some(v) => {
                self.forget_version(bucket, key, &v)?;
                Ok(DeleteSpecificResult { found: true, was_delete_marker: v.is_delete_marker, version_id: v.shown_id() })
            }
            None => Ok(DeleteSpecificResult { found: false, was_delete_marker: false, version_id: version_id.to_string() }),
        }
    }

    fn get_object_version(&self, user_id: &str, bucket: &str, key: &str, version_id: &str) -> Result<Metadata, Error> {
        let o = self.object(user_id, bucket, key).ok_or_else(|| not_found(bucket, key))?;
        let v = o.find(version_id).map(|i| &o.versions[i]).ok_or_else(|| not_found(bucket, key))?;
        let mut m = v.meta.clone();
        m.is_delete_marker = v.is_delete_marker;
        m.version_id = Some(v.shown_id());
        Ok(m)
    }

    fn get_latest_last_modified(&self, user_id: &str, bucket: &str, key: &str) -> Result<Option<String>, Error> {
        Ok(self.latest(user_id, bucket, key).and_then(|v| v.meta.last_modified))
    }

    fn get_latest_version_marker_info(&self, user_id: &str, bucket: &str, key: &str) -> Result<Option<(String, bool)>, Error> {
        Ok(self.latest(user_id, bucket, key).map(|v| (v.version_id, v.is_delete_marker)))
    }

    /// Versions in key order, newest first within a key, starting after
    /// `(key_marker, version_id_marker)`.
    fn list_object_versions_full(
        &self, user_id: &str, bucket: &str, prefix: &str, key_marker: &str, version_id_marker: &str, max_keys: usize,
    ) -> Result<(Vec<VersionRow>, bool, String, String), Error> {
        let mut rows = Vec::new();
        for (key, o) in self.objects_in(user_id, bucket) {
            if !key.starts_with(prefix) || (!key_marker.is_empty() && key.as_str() < key_marker) {
                continue;
            }
            let latest_idx = o.versions.len() - 1;
            let mut newest_first: Vec<(usize, &VersionEntry)> = o.versions.iter().enumerate().rev().collect();
            if !key_marker.is_empty() && key == key_marker {
                if version_id_marker.is_empty() {
                    continue;
                }
                match newest_first.iter().position(|(_, v)| v.shown_id() == version_id_marker || v.version_id == version_id_marker) {
                    Some(p) => newest_first = newest_first.split_off(p + 1),
                    None => continue,
                }
            }
            for (i, v) in newest_first {
                rows.push(VersionRow {
                    key: key.clone(),
                    version_id: v.shown_id(),
                    is_delete_marker: v.is_delete_marker,
                    etag: v.meta.etag.clone().unwrap_or_default(),
                    size: if v.is_delete_marker { 0 } else { v.meta.size },
                    last_modified: v.meta.last_modified.clone().unwrap_or_default(),
                    is_latest: i == latest_idx,
                });
            }
        }
        let truncated = max_keys > 0 && rows.len() > max_keys;
        if truncated {
            rows.truncate(max_keys);
        }
        let (next_key, next_vid) = match (truncated, rows.last()) {
            (true, Some(last)) => (last.key.clone(), last.version_id.clone()),
            _ => (String::new(), String::new()),
        };
        Ok((rows, truncated, next_key, next_vid))
    }

    // ---- tagging ----

    fn set_object_tags(&self, user_id: &str, bucket: &str, key: &str, tags: &[(String, String)]) -> Result<(), Error> {
        self.update_object(user_id, bucket, key, |o| match o.versions.last_mut() {
            Some(v) => {
                // Kept sorted by key: GetObjectTagging returns them in key
                // order whatever order the PUT sent them in.
                let mut sorted = tags.to_vec();
                sorted.sort();
                v.tags = sorted;
                Ok(())
            }
            None => Err(not_found(bucket, key)),
        })
    }
    fn get_object_tags(&self, user_id: &str, bucket: &str, key: &str) -> Result<Vec<(String, String)>, Error> {
        Ok(self.latest(user_id, bucket, key).map(|v| v.tags).unwrap_or_default())
    }
    fn delete_object_tags(&self, user_id: &str, bucket: &str, key: &str) -> Result<(), Error> {
        if self.latest(user_id, bucket, key).is_none() {
            return Ok(());
        }
        self.set_object_tags(user_id, bucket, key, &[])
    }
    fn get_object_tag_count(&self, user_id: &str, bucket: &str, key: &str) -> Result<i64, Error> {
        Ok(self.get_object_tags(user_id, bucket, key)?.len() as i64)
    }
    fn set_bucket_tags(&self, bucket: &str, tags: &[(String, String)]) -> Result<(), Error> {
        self.update_bucket(bucket, |b| {
            b.tags = tags.to_vec();
            b.tags.sort();
        })
    }
    fn get_bucket_tags(&self, bucket: &str) -> Result<Vec<(String, String)>, Error> {
        Ok(self.bucket(bucket).map(|b| b.tags).unwrap_or_default())
    }
    fn delete_bucket_tags(&self, bucket: &str) -> Result<(), Error> {
        if self.bucket(bucket).is_none() {
            return Ok(());
        }
        self.update_bucket(bucket, |b| b.tags.clear())
    }
    fn set_multipart_tagging(&self, upload_id: &str, tagging: &str) -> Result<(), Error> {
        self.update_upload(upload_id, |u| u.tagging = tagging.to_string())
    }
    fn get_multipart_tagging(&self, upload_id: &str) -> Result<String, Error> {
        Ok(self.upload(upload_id).map(|u| u.tagging).unwrap_or_default())
    }

    // ---- ACL ----

    fn set_bucket_acl(&self, bucket: &str, owner_id: &str, grants_json: &str) -> Result<(), Error> {
        self.update_bucket(bucket, |b| b.acl = Some((owner_id.to_string(), grants_json.to_string())))
    }
    fn get_bucket_acl(&self, bucket: &str) -> Result<Option<(String, String)>, Error> {
        Ok(self.bucket(bucket).and_then(|b| b.acl))
    }
    fn set_object_acl(&self, bucket: &str, key: &str, version_id: &str, owner_id: &str, grants_json: &str) -> Result<(), Error> {
        let rk = version_scoped("acl", bucket, key, version_id);
        let _g = self.locks.lock(&rk);
        self.put(&rk, MetaValue::Acl { owner: owner_id.to_string(), grants: grants_json.to_string() })
    }
    fn get_object_acl(&self, bucket: &str, key: &str, version_id: &str) -> Result<Option<(String, String)>, Error> {
        Ok(match self.get(&version_scoped("acl", bucket, key, version_id)) {
            Some(MetaValue::Acl { owner, grants }) => Some((owner, grants)),
            _ => None,
        })
    }
    fn set_public_access_block(&self, bucket: &str, a: bool, b: bool, c: bool, d: bool) -> Result<(), Error> {
        self.update_bucket(bucket, |row| row.public_access_block = Some((a, b, c, d)))
    }
    fn get_public_access_block(&self, bucket: &str) -> Result<Option<(bool, bool, bool, bool)>, Error> {
        Ok(self.bucket(bucket).and_then(|b| b.public_access_block))
    }
    fn delete_public_access_block(&self, bucket: &str) -> Result<(), Error> {
        if self.bucket(bucket).is_none() {
            return Ok(());
        }
        self.update_bucket(bucket, |b| b.public_access_block = None)
    }

    // ---- multipart ----

    fn create_multipart_upload(
        &self, upload_id: &str, user_id: &str, bucket: &str, key: &str, content_type: Option<&str>, metadata_json: &str,
        initiated_at: &str, checksum_algorithm: &str, checksum_type: &str, object_lock_mode: &str,
        object_lock_retain_until: &str, object_lock_legal_hold: &str,
    ) -> Result<(), Error> {
        let rk = upload_key(upload_id);
        let _g = self.locks.lock(&rk);
        if self.upload(upload_id).is_some() {
            return Ok(());
        }
        self.put(
            &rk,
            MetaValue::Upload(UploadRow {
                user_id: user_id.to_string(),
                bucket: bucket.to_string(),
                key: key.to_string(),
                content_type: content_type.map(str::to_string),
                metadata_json: metadata_json.to_string(),
                initiated_at: initiated_at.to_string(),
                status: "in_progress".to_string(),
                final_etag: None,
                checksum_algorithm: checksum_algorithm.to_string(),
                checksum_type: checksum_type.to_string(),
                object_lock_mode: object_lock_mode.to_string(),
                object_lock_retain_until: object_lock_retain_until.to_string(),
                object_lock_legal_hold: object_lock_legal_hold.to_string(),
                tagging: String::new(),
                parts: BTreeMap::new(),
            }),
        )
    }

    fn get_multipart_upload(&self, upload_id: &str) -> Result<Option<MultipartUploadRow>, Error> {
        Ok(self.upload(upload_id).map(|u| upload_view(upload_id, &u)))
    }

    fn mark_multipart_completed(&self, upload_id: &str, final_etag: &str) -> Result<(), Error> {
        if self.upload(upload_id).is_none() {
            return Ok(());
        }
        self.update_upload(upload_id, |u| {
            u.status = "completed".to_string();
            u.final_etag = Some(final_etag.to_string());
        })
    }

    fn delete_multipart_upload(&self, upload_id: &str) -> Result<(), Error> {
        let rk = upload_key(upload_id);
        let _g = self.locks.lock(&rk);
        self.remove(&rk)
    }

    fn delete_completed_uploads_for_key(&self, bucket: &str, key: &str) -> Result<(), Error> {
        let done: Vec<String> = self
            .rows
            .list_live("upl/", |k, r| match &r.value {
                MetaValue::Upload(u) if u.bucket == bucket && u.key == key && u.status == "completed" => Some(k["upl/".len()..].to_string()),
                _ => None,
            })
            .into_iter()
            .flatten()
            .collect();
        for id in done {
            self.delete_multipart_upload(&id)?;
        }
        Ok(())
    }

    fn list_bucket_multipart_uploads(&self, bucket: &str) -> Result<Vec<MultipartUploadRow>, Error> {
        let mut out: Vec<MultipartUploadRow> = self
            .rows
            .list_live("upl/", |k, r| match &r.value {
                MetaValue::Upload(u) if u.bucket == bucket && u.status == "in_progress" => Some(upload_view(&k["upl/".len()..], u)),
                _ => None,
            })
            .into_iter()
            .flatten()
            .collect();
        out.sort_by(|a, b| (&a.key, &a.initiated_at, &a.upload_id).cmp(&(&b.key, &b.initiated_at, &b.upload_id)));
        Ok(out)
    }

    fn upsert_multipart_part(&self, upload_id: &str, part_number: i32, etag: &str, size: u64, extents_blob: &[u8], checksum_value: &str) -> Result<(), Error> {
        let extents = crate::util::serializer::deserialize_offset_size(extents_blob)?;
        let mut replaced = Vec::new();
        self.update_upload(upload_id, |u| {
            let blobs = blobs_for(&u.user_id, &u.bucket, &extents);
            let part = PartRow { etag: etag.to_string(), size, extents_blob: extents_blob.to_vec(), checksum_value: checksum_value.to_string(), blobs };
            if let Some(old) = u.parts.insert(part_number, part) {
                replaced = old.blobs;
            }
        })?;
        // A part uploaded again under the same number: the old one is dead.
        self.gc(replaced)
    }

    fn list_multipart_parts(&self, upload_id: &str) -> Result<Vec<MultipartPartRow>, Error> {
        Ok(self
            .upload(upload_id)
            .map(|u| {
                u.parts
                    .into_iter()
                    .map(|(n, p)| MultipartPartRow { part_number: n, etag: p.etag, size: p.size, extents_blob: p.extents_blob, checksum_value: p.checksum_value })
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Clears an upload's parts. A part blob the object's current version
    /// holds moved there (CompleteMultipartUpload): it lives on. Every
    /// other one is dead: all of them on an abort, the parts left out on a
    /// completion.
    fn delete_parts_for_upload(&self, upload_id: &str) -> Result<(), Error> {
        let Some(up) = self.upload(upload_id) else {
            return Ok(());
        };
        let kept: std::collections::HashSet<String> =
            self.latest(&up.user_id, &up.bucket, &up.key).map(|v| v.blobs.iter().map(|b| b.placement.wire_key.clone()).collect()).unwrap_or_default();
        let mut dead = Vec::new();
        self.update_upload(upload_id, |u| {
            dead.extend(u.parts.values().flat_map(|p| p.blobs.iter()).filter(|b| !kept.contains(&b.placement.wire_key)).cloned());
            u.parts.clear();
        })?;
        self.gc(dead)
    }

    fn get_parts_manifest(&self, user_id: &str, bucket: &str, key: &str) -> Result<Option<String>, Error> {
        Ok(self.latest(user_id, bucket, key).and_then(|v| v.parts_manifest))
    }

    fn set_parts_manifest(&self, user_id: &str, bucket: &str, key: &str, manifest: &str) -> Result<(), Error> {
        self.update_object(user_id, bucket, key, |o| match o.versions.last_mut() {
            Some(v) => {
                v.parts_manifest = Some(manifest.to_string());
                Ok(())
            }
            None => Err(not_found(bucket, key)),
        })
    }

    // ---- Object Lock ----

    fn get_bucket_object_lock_enabled(&self, bucket: &str) -> Result<bool, Error> {
        Ok(self.bucket(bucket).is_some_and(|b| b.lock_enabled))
    }

    fn set_bucket_object_lock_enabled(&self, bucket: &str, enabled: bool) -> Result<(), Error> {
        self.update_bucket(bucket, |b| b.lock_enabled = enabled)
    }

    fn create_bucket_with_lock(&self, user_id: &str, bucket: &str, lock_enabled: bool) -> Result<(), Error> {
        let key = bucket_key(bucket);
        let _g = self.locks.lock(&key);
        if self.bucket(bucket).is_some() {
            return Ok(());
        }
        self.put(
            &key,
            MetaValue::Bucket(BucketRow {
                owner: user_id.to_string(),
                created_at: now_iso(),
                // An Object Lock bucket is versioned from creation, as in S3.
                versioning: if lock_enabled { "enabled".to_string() } else { "disabled".to_string() },
                lock_enabled,
                ..Default::default()
            }),
        )
    }

    fn get_object_lock_config(&self, bucket: &str) -> Result<Option<(String, Option<i64>, Option<i64>)>, Error> {
        Ok(self.bucket(bucket).and_then(|b| b.lock_config))
    }

    fn put_object_lock_config(&self, bucket: &str, mode: &str, days: Option<i64>, years: Option<i64>) -> Result<(), Error> {
        self.update_bucket(bucket, |b| b.lock_config = Some((mode.to_string(), days, years)))
    }

    fn get_object_lock(&self, bucket: &str, key: &str, version_id: &str) -> Result<Option<ObjectLockRow>, Error> {
        Ok(self.lock_row(bucket, key, version_id).map(|l| ObjectLockRow { mode: l.mode, retain_until_date: l.retain_until_date, legal_hold: l.legal_hold }))
    }

    /// A `None` field keeps its current value. A new row's legal hold is OFF.
    fn put_object_lock(&self, bucket: &str, key: &str, version_id: &str, mode: Option<&str>, retain_until_date: Option<&str>, legal_hold: Option<&str>) -> Result<(), Error> {
        let rk = version_scoped("lck", bucket, key, version_id);
        let _g = self.locks.lock(&rk);
        let mut row = self.lock_row(bucket, key, version_id).unwrap_or(LockRow { mode: None, retain_until_date: None, legal_hold: "OFF".to_string() });
        if let Some(m) = mode {
            row.mode = Some(m.to_string());
        }
        if let Some(r) = retain_until_date {
            row.retain_until_date = Some(r.to_string());
        }
        if let Some(h) = legal_hold {
            row.legal_hold = h.to_string();
        }
        self.put(&rk, MetaValue::Lock(row))
    }

    fn set_object_legal_hold(&self, bucket: &str, key: &str, version_id: &str, status: &str) -> Result<(), Error> {
        self.put_object_lock(bucket, key, version_id, None, None, Some(status))
    }

    fn check_object_lock_protection(&self, bucket: &str, key: &str, version_id: &str, bypass_governance: bool) -> Result<(bool, bool), Error> {
        let Some(l) = self.lock_row(bucket, key, version_id) else {
            return Ok((false, false));
        };
        let in_force = l
            .retain_until_date
            .as_deref()
            .and_then(|d| chrono::DateTime::parse_from_rfc3339(d).ok())
            .is_some_and(|until| until > chrono::Utc::now());
        let retention = in_force
            && match l.mode.as_deref() {
                Some("COMPLIANCE") => true,
                Some("GOVERNANCE") => !bypass_governance,
                _ => false,
            };
        Ok((retention, l.legal_hold == "ON"))
    }
}

fn upload_view(upload_id: &str, u: &UploadRow) -> MultipartUploadRow {
    MultipartUploadRow {
        upload_id: upload_id.to_string(),
        user_id: u.user_id.clone(),
        bucket: u.bucket.clone(),
        key: u.key.clone(),
        content_type: u.content_type.clone(),
        metadata_json: u.metadata_json.clone(),
        initiated_at: u.initiated_at.clone(),
        status: u.status.clone(),
        final_etag: u.final_etag.clone(),
        checksum_algorithm: u.checksum_algorithm.clone(),
        checksum_type: u.checksum_type.clone(),
        object_lock_mode: u.object_lock_mode.clone(),
        object_lock_retain_until: u.object_lock_retain_until.clone(),
        object_lock_legal_hold: u.object_lock_legal_hold.clone(),
    }
}

/// The metadata store's tests, run once per engine: the same rows, locks
/// and restarts must behave the same on the KV and the SQL engine.
#[cfg(test)]
macro_rules! metadata_store_tests {
    ($name:ident, $kind:expr) => {
        mod $name {
            use super::*;
            const KIND: MetaEngineKind = $kind;

    fn store() -> (tempfile::TempDir, ReplicatedMetadataStore) {
        let dir = tempfile::tempdir().unwrap();
        let rows = Arc::new(open_rows(KIND, &dir.path().join("m.log").to_string_lossy()).unwrap());
        let s = ReplicatedMetadataStore::new(Arc::clone(&rows), Box::new(LocalWriter(rows)));
        (dir, s)
    }

    fn meta(size: u64, etag: &str) -> Metadata {
        let mut m = Metadata::from_offset_size_list(vec![(1000, size)]);
        m.etag = Some(etag.into());
        m.last_modified = Some(now_iso());
        m
    }

    #[test]
    fn an_unversioned_overwrite_replaces_and_returns_the_old_extents() {
        let (_d, s) = store();
        s.create_bucket("u", "b").unwrap();
        assert_eq!(s.put_object_v2("u", "b", "k", &meta(5, "a")).unwrap(), (None, vec![]));
        assert_eq!(s.put_object_v2("u", "b", "k", &meta(7, "b")).unwrap(), (None, vec![(1000, 5)]));
        assert_eq!(s.get_metadata("u", "b", "k").unwrap().etag.as_deref(), Some("b"));
        assert_eq!(s.list_objects("u", "b").unwrap(), vec!["k".to_string()]);
        assert!(matches!(s.delete_object_v2("u", "b", "k").unwrap(), VersioningDeleteResult::Deleted));
    }

    #[test]
    fn versioned_puts_keep_every_version_and_a_delete_adds_a_marker() {
        let (_d, s) = store();
        s.create_bucket("u", "b").unwrap();
        s.put_object_v2("u", "b", "k", &meta(1, "pre")).unwrap();
        s.set_versioning_state("u", "b", "enabled").unwrap();
        let (v1, gc1) = s.put_object_v2("u", "b", "k", &meta(2, "v1")).unwrap();
        let (v2, _) = s.put_object_v2("u", "b", "k", &meta(3, "v2")).unwrap();
        assert!(gc1.is_empty() && v1.is_some() && v2.is_some() && v1 != v2);
        assert_eq!(s.get_object_version("u", "b", "k", "null").unwrap().etag.as_deref(), Some("pre"));
        let VersioningDeleteResult::Marker { version_id: marker } = s.delete_object_v2("u", "b", "k").unwrap() else { panic!() };
        assert!(s.get_metadata("u", "b", "k").is_err(), "a delete marker hides the object");
        assert_eq!(s.get_latest_version_marker_info("u", "b", "k").unwrap(), Some((marker.clone(), true)));
        let (rows, truncated, _, _) = s.list_object_versions_full("u", "b", "", "", "", 1000).unwrap();
        let ids: Vec<&str> = rows.iter().map(|r| r.version_id.as_str()).collect();
        assert_eq!(ids, vec![marker.as_str(), v2.as_deref().unwrap(), v1.as_deref().unwrap(), "null"]);
        assert!(!truncated && rows[0].is_latest && rows[0].is_delete_marker);
        // Removing the marker brings v2 back.
        assert!(s.delete_specific_version("u", "b", "k", &marker).unwrap().was_delete_marker);
        assert_eq!(s.get_metadata("u", "b", "k").unwrap().etag.as_deref(), Some("v2"));
    }

    #[test]
    fn suspended_versioning_replaces_only_the_null_version() {
        let (_d, s) = store();
        s.create_bucket("u", "b").unwrap();
        s.set_versioning_state("u", "b", "enabled").unwrap();
        let (v1, _) = s.put_object_v2("u", "b", "k", &meta(1, "v1")).unwrap();
        s.set_versioning_state("u", "b", "suspended").unwrap();
        assert_eq!(s.put_object_v2("u", "b", "k", &meta(2, "n1")).unwrap().0.as_deref(), Some("null"));
        assert_eq!(s.put_object_v2("u", "b", "k", &meta(3, "n2")).unwrap(), (Some("null".into()), vec![(1000, 2)]));
        let (rows, ..) = s.list_object_versions_full("u", "b", "", "", "", 1000).unwrap();
        assert_eq!(rows.iter().map(|r| r.version_id.clone()).collect::<Vec<_>>(), vec!["null".to_string(), v1.unwrap()]);
    }

    #[test]
    fn version_listing_pages_by_key_and_version_marker() {
        let (_d, s) = store();
        s.create_bucket("u", "b").unwrap();
        s.set_versioning_state("u", "b", "enabled").unwrap();
        for k in ["a", "a", "b"] {
            s.put_object_v2("u", "b", k, &meta(1, k)).unwrap();
        }
        let (page1, truncated, nk, nv) = s.list_object_versions_full("u", "b", "", "", "", 2).unwrap();
        assert!(truncated && page1.len() == 2 && nk == "a");
        let (page2, truncated, ..) = s.list_object_versions_full("u", "b", "", &nk, &nv, 2).unwrap();
        assert!(!truncated);
        assert_eq!(page2.iter().map(|r| r.key.as_str()).collect::<Vec<_>>(), vec!["b"]);
    }

    #[test]
    fn prefix_listing_shows_each_live_key_once_and_hides_deleted_ones() {
        let (_d, s) = store();
        s.create_bucket("u", "b").unwrap();
        s.set_versioning_state("u", "b", "enabled").unwrap();
        for k in ["a", "ab", "ab", "ab/c", "b"] {
            s.put_object_v2("u", "b", k, &meta(1, k)).unwrap();
        }
        s.delete_object_v2("u", "b", "ab/c").unwrap();
        assert_eq!(s.list_objects_with_prefix("u", "b", "a").unwrap(), vec!["a".to_string(), "ab".to_string()]);
        assert_eq!(s.list_objects_with_prefix("u", "b", "ab").unwrap(), vec!["ab".to_string()], "ab/c's latest is a delete marker");
        assert_eq!(s.list_objects("u", "b").unwrap(), vec!["a".to_string(), "ab".to_string(), "b".to_string()]);
    }

    #[test]
    fn buckets_belong_to_their_owner() {
        let (_d, s) = store();
        s.create_bucket("alice", "b1").unwrap();
        s.create_bucket("bob", "b2").unwrap();
        assert!(s.bucket_exists("alice", "b1").unwrap() && !s.bucket_exists("bob", "b1").unwrap());
        assert_eq!(s.list_all_buckets_for_user("alice").unwrap(), vec!["b1".to_string()]);
        s.delete_bucket("bob", "b1").unwrap();
        assert!(s.bucket_exists("alice", "b1").unwrap(), "another user can't delete it");
        s.delete_bucket("alice", "b1").unwrap();
        assert!(!s.bucket_exists("alice", "b1").unwrap());
    }

    #[test]
    fn bucket_settings_round_trip() {
        let (_d, s) = store();
        s.create_bucket("u", "b").unwrap();
        s.set_bucket_cors("b", "<CORS/>").unwrap();
        s.set_bucket_tags("b", &[("t".into(), "1".into())]).unwrap();
        s.set_bucket_acl("b", "u", "[]").unwrap();
        s.set_public_access_block("b", true, false, true, false).unwrap();
        s.put_object_lock_config("b", "GOVERNANCE", Some(1), None).unwrap();
        assert_eq!(s.get_bucket_cors("b").unwrap().as_deref(), Some("<CORS/>"));
        assert_eq!(s.get_bucket_tags("b").unwrap().len(), 1);
        assert_eq!(s.get_public_access_block("b").unwrap(), Some((true, false, true, false)));
        assert_eq!(s.get_object_lock_config("b").unwrap(), Some(("GOVERNANCE".into(), Some(1), None)));
        s.delete_bucket_cors("b").unwrap();
        assert_eq!(s.get_bucket_cors("b").unwrap(), None);
    }

    #[test]
    fn an_overwrite_starts_with_no_tags_and_no_manifest() {
        let (_d, s) = store();
        s.create_bucket("u", "b").unwrap();
        s.put_object_v2("u", "b", "k", &meta(1, "a")).unwrap();
        s.set_object_tags("u", "b", "k", &[("x".into(), "y".into())]).unwrap();
        s.set_parts_manifest("u", "b", "k", "[1]").unwrap();
        assert_eq!(s.get_object_tag_count("u", "b", "k").unwrap(), 1);
        s.put_object_v2("u", "b", "k", &meta(2, "b")).unwrap();
        assert_eq!(s.get_object_tag_count("u", "b", "k").unwrap(), 0);
        assert_eq!(s.get_parts_manifest("u", "b", "k").unwrap(), None);
    }

    #[test]
    fn multipart_uploads_and_parts() {
        let (_d, s) = store();
        s.create_multipart_upload("up1", "u", "b", "k", None, "{}", "t", "", "", "", "", "").unwrap();
        let ext = |o: u64, n: u64| crate::util::serializer::serialize_offset_size(&vec![(o, n)]).unwrap();
        s.upsert_multipart_part("up1", 2, "e2", 20, &ext(100, 20), "").unwrap();
        s.upsert_multipart_part("up1", 1, "e1", 10, &ext(0, 10), "").unwrap();
        s.upsert_multipart_part("up1", 1, "e1b", 11, &ext(50, 11), "").unwrap();
        let parts = s.list_multipart_parts("up1").unwrap();
        assert_eq!(parts.iter().map(|p| (p.part_number, p.etag.as_str())).collect::<Vec<_>>(), vec![(1, "e1b"), (2, "e2")]);
        assert_eq!(s.list_bucket_multipart_uploads("b").unwrap().len(), 1);
        s.mark_multipart_completed("up1", "final").unwrap();
        assert!(s.list_bucket_multipart_uploads("b").unwrap().is_empty(), "only in-progress uploads are listed");
        s.delete_completed_uploads_for_key("b", "k").unwrap();
        assert!(s.get_multipart_upload("up1").unwrap().is_none());
    }

    #[test]
    fn object_lock_merges_fields_and_enforces_retention() {
        let (_d, s) = store();
        let future = (chrono::Utc::now() + chrono::Duration::days(1)).to_rfc3339();
        s.put_object_lock("b", "k", "v1", Some("GOVERNANCE"), Some(&future), None).unwrap();
        assert_eq!(s.get_object_lock("b", "k", "v1").unwrap().unwrap().legal_hold, "OFF");
        s.set_object_legal_hold("b", "k", "v1", "ON").unwrap();
        let l = s.get_object_lock("b", "k", "v1").unwrap().unwrap();
        assert_eq!((l.mode.as_deref(), l.legal_hold.as_str()), (Some("GOVERNANCE"), "ON"), "the hold kept the retention");
        assert_eq!(s.check_object_lock_protection("b", "k", "v1", false).unwrap(), (true, true));
        assert_eq!(s.check_object_lock_protection("b", "k", "v1", true).unwrap(), (false, true));
    }

    #[test]
    fn the_rows_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.log").to_string_lossy().into_owned();
        {
            let rows = Arc::new(open_rows(KIND, &path).unwrap());
            let s = ReplicatedMetadataStore::new(Arc::clone(&rows), Box::new(LocalWriter(rows)));
            s.create_bucket("u", "b").unwrap();
            s.put_object_v2("u", "b", "k", &meta(4, "e")).unwrap();
            // An upload's parts are a map keyed by part number.
            s.create_multipart_upload("up", "u", "b", "k2", None, "{}", "t", "", "", "", "", "").unwrap();
            let ext = crate::util::serializer::serialize_offset_size(&vec![(0, 10)]).unwrap();
            s.upsert_multipart_part("up", 3, "e3", 10, &ext, "").unwrap();
        }
        let rows = Arc::new(open_rows(KIND, &path).unwrap());
        let s = ReplicatedMetadataStore::new(Arc::clone(&rows), Box::new(LocalWriter(rows)));
        assert_eq!(s.get_metadata("u", "b", "k").unwrap().size, 4);
        assert_eq!(s.list_multipart_parts("up").unwrap()[0].part_number, 3);
    }
        }
    };
}

#[cfg(test)]
metadata_store_tests!(tests_bitcask, MetaEngineKind::Bitcask);
#[cfg(test)]
metadata_store_tests!(tests_sqlite, MetaEngineKind::Sqlite);
