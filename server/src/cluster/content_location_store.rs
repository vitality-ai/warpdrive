//! Location store for content-dependent (multi-stripe) objects — the same
//! Bitcask append-only-log pattern as `location_store.rs`, applied to a
//! richer record shape: one object here maps to many independently
//! erasure-coded stripes (one per `StripePacker::pack` output), not one
//! shard set. Kept as a separate store/trait rather than extending
//! `LocationRecord` itself, since the two shapes (one peer set vs. many,
//! plus the unit-position index needed to reassemble original byte order)
//! are different enough that forcing one schema to cover both would mean
//! optional fields on every single-stripe object for a feature most
//! objects never use.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

/// One stripe's placement pin: which peers hold its `k+m` shards, the
/// shared bin size (`capacity`, what every shard in this stripe is padded
/// to), and which unit_ids landed in which of the `k` bins, in pack order
/// (needed to reassemble a bin's concatenated bytes back into individual
/// units).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StripeRecord {
    pub shard_peers: Vec<String>,
    pub capacity: usize,
    pub bins: Vec<Vec<String>>,
}

/// One computable unit's position in the original object plus what's
/// needed to run pushdown against it. `len` is the unit's stored
/// (compressed) byte length; `uncompressed_len` and `codec` are only
/// meaningful for pushdown-capable units — a unit built without codec
/// information (the plain storage/placement path, most callers) gets
/// `codec: "opaque"` and `uncompressed_len == len`, i.e. compressibility
/// 1.0, which the cost equation (see `pushdown.rs`) naturally treats as
/// "never worth pushing down a projection for, filtering is still fine."
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UnitMeta {
    pub unit_id: String,
    pub offset: u64,
    pub len: u64,
    pub uncompressed_len: u64,
    pub codec: String,
    /// Opaque bytes passed straight through to `packing::Unit::metadata` at
    /// pack time — see that field's doc for why (workload-aware packers
    /// need more than a unit's size). `#[serde(default)]` so records
    /// written before this field existed still replay from the Bitcask log.
    #[serde(default)]
    pub metadata: Vec<u8>,
}

/// One unit's exact location in the stripe/bin layout: which stripe owns
/// it, which bin within that stripe, and its byte position within that
/// bin's concatenated (pre-padding) data -- everything needed to copy its
/// bytes straight out of a fetched stripe's bins, with no unit_id string
/// lookup of any kind.
///
/// Computed once, at PUT time, when bin assignments are already known
/// (see `packed.rs`'s write path), and stored sorted by `offset` as part
/// of the persisted record -- not rebuilt, or cached, on every GET. This
/// mirrors how a real object store (MinIO: `FileInfo.Parts`, computed at
/// write time) handles the same problem for its own fixed-size erasure
/// blocks: compute layout once when you already have the information,
/// store it, do cheap arithmetic/binary-search against the stored result
/// at read time. Content-dependent packing has no fixed-block-size
/// shortcut to fall back on (units are grouped by content similarity, not
/// position), so this field is the equivalent of that stored layout for
/// this packer class specifically.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UnitLocation {
    pub offset: u64,
    pub end: u64,
    pub stripe_index: usize,
    pub bin_idx: usize,
    pub pos_in_bin: usize,
    pub len: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ContentDependentRecord {
    /// See `location_store::UNAUTHENTICATED_USER`. `#[serde(default)]`:
    /// empty for a record written before this field existed.
    #[serde(default)]
    pub user_id: String,
    pub bucket: String,
    pub key: String,
    pub k: usize,
    pub m: usize,
    pub original_len: usize,
    /// Each unit's position in the *original* object, so a whole-object GET
    /// can place each unit's recovered bytes back where they came from —
    /// plus its codec info for pushdown (see `UnitMeta`).
    pub units: Vec<UnitMeta>,
    pub stripes: Vec<StripeRecord>,
    /// Same purpose and same mechanism as `LocationRecord::version`
    /// (#151): every stripe's shards are written under
    /// `shard_storage::versioned_key(stripe_key(key, i), version)`, not a
    /// plain, reused key, so an overwrite's shard writes never land on a
    /// key this pin (or any in-flight reader still using it) is relying
    /// on. `#[serde(default)]`: empty means a record from before this
    /// field existed, whose shards are at the old, unversioned key.
    #[serde(default)]
    pub version: String,
    /// True when the caller's own `units_header` left part of the body
    /// uncovered (`verify_stripes_reconstruct_original` allows this --
    /// see its doc -- a legitimate choice for a caller like a row-group
    /// reader that only ever wants specific columns). A GET zero-fills
    /// whatever isn't covered, which is fine when it's intentional and
    /// silent corruption when it isn't (e.g. a client's own offset
    /// computation missed a Parquet footer). Not rejected at PUT time --
    /// that would break the legitimate case -- but surfaced instead of
    /// hidden: logged as a warning at PUT time, and every GET response
    /// carries an `X-Warpd-Sparse-Coverage: true` header when this is
    /// set, so "this object returned 200 but isn't fully covered" is
    /// observable rather than indistinguishable from a healthy object.
    /// `#[serde(default)]`: false for a record from before this field
    /// existed, correct either way since the old code path required full
    /// coverage, so no pre-existing record can actually have a gap.
    #[serde(default)]
    pub has_sparse_gaps: bool,
    /// See `UnitLocation`. `#[serde(default)]`: empty means a record
    /// written before this field existed; callers fall back to the old
    /// units-by-id + per-stripe scan in that case rather than failing.
    #[serde(default)]
    pub sorted_unit_locations: Vec<UnitLocation>,
    /// Same S3-metadata fields as `LocationRecord`, same reasoning and
    /// same additive `#[serde(default)]` treatment -- see its doc.
    #[serde(default)]
    pub etag: Option<String>,
    #[serde(default)]
    pub content_type: Option<String>,
    #[serde(default)]
    pub last_modified: Option<String>,
    #[serde(default)]
    pub user_metadata: HashMap<String, String>,
    #[serde(default)]
    pub cache_control: Option<String>,
    #[serde(default)]
    pub expires: Option<String>,
    #[serde(default)]
    pub content_encoding: Option<String>,
    #[serde(default)]
    pub checksum_algorithm: Option<String>,
    #[serde(default)]
    pub checksum_value: Option<String>,
    #[serde(default)]
    pub checksum_type: Option<String>,
    /// S3 object tags, same shape and same `#[serde(default)]` reasoning
    /// as `LocationRecord::tags`.
    #[serde(default)]
    pub tags: Vec<(String, String)>,
}

impl ContentDependentRecord {
    /// Every unit overlapping `[start, end]` (inclusive), via binary
    /// search over `sorted_unit_locations` rather than a linear scan.
    /// Units tile the original object without overlapping each other, so
    /// the overlapping set is one contiguous run in sorted order: found
    /// with a binary search for the run's start, then a scan only over
    /// that run, not all units.
    pub fn units_overlapping(&self, start: u64, end: u64) -> &[UnitLocation] {
        let begin = self.sorted_unit_locations.partition_point(|u| u.end <= start);
        let rel_end = self.sorted_unit_locations[begin..].partition_point(|u| u.offset <= end);
        &self.sorted_unit_locations[begin..begin + rel_end]
    }
}

impl ContentDependentRecord {
    /// Finds which stripe and which bin within it holds `unit_id` — the
    /// lookup a pushdown query needs before it knows which single peer to
    /// route to. Small linear scan; objects have at most a few hundred
    /// computable units in practice (Table 3 of the Fusion paper: 84-320),
    /// so this is not worth indexing.
    pub fn locate_unit(&self, unit_id: &str) -> Option<(usize, usize)> {
        for (stripe_idx, stripe) in self.stripes.iter().enumerate() {
            for (bin_idx, bin) in stripe.bins.iter().enumerate() {
                if bin.iter().any(|id| id == unit_id) {
                    return Some((stripe_idx, bin_idx));
                }
            }
        }
        None
    }
}

pub trait ContentLocationStore: Send + Sync {
    fn put(&self, record: ContentDependentRecord) -> io::Result<()>;
    fn get(&self, user_id: &str, bucket: &str, key: &str) -> Option<ContentDependentRecord>;
    fn delete(&self, user_id: &str, bucket: &str, key: &str) -> io::Result<()>;
    /// See `LocationStore::list` — same minimal-S3-surface purpose, for the
    /// content-dependent half of a bucket's objects.
    fn list(&self, user_id: &str, bucket: &str, prefix: &str) -> Vec<(String, u64)>;
    /// See `LocationStore::compact` — same reasoning and same mechanism
    /// (#163), applied to this store's own log.
    fn compact(&self) -> io::Result<()>;
}

/// Length-prefixed, same scheme and same reasoning as `location_store.rs`'s
/// `record_key` (#150: a plain `/` separator lets bucket `a/b` + key `c`
/// and bucket `a` + key `b/c` collide onto the same index entry; the same
/// risk applies one level up to `user_id`, so it gets the same treatment).
fn record_key(user_id: &str, bucket: &str, key: &str) -> String {
    format!("{}:{}:{}:{}:{}", user_id.len(), user_id, bucket.len(), bucket, key)
}

/// `BTreeMap`, not `HashMap` (#163) — see `location_store.rs`'s matching
/// doc comment, same reasoning: `record_key`'s length-prefixed bucket
/// still groups each bucket's keys contiguously, so lexicographic order
/// makes `list`'s prefix lookup a bounded range scan.
pub struct BitcaskContentLocationStore {
    log_path: PathBuf,
    log_file: RwLock<File>,
    index: RwLock<BTreeMap<String, ContentDependentRecord>>,
    /// #155 — see `location_store.rs`'s matching field doc for why this
    /// is needed: without one lock held across both the log append and
    /// the index update, a concurrent put/delete for the same key can
    /// apply to the log and the index in different orders, so a restart
    /// (which only ever replays the log) can disagree with what the live
    /// index had.
    write_lock: std::sync::Mutex<()>,
}

impl BitcaskContentLocationStore {
    pub fn open(log_path: impl AsRef<Path>) -> io::Result<Self> {
        let log_path: PathBuf = log_path.as_ref().to_path_buf();
        if let Some(parent) = log_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut index = BTreeMap::new();
        if log_path.exists() {
            let f = File::open(&log_path)?;
            let lines: Vec<String> = BufReader::new(f).lines().collect::<io::Result<_>>()?;
            let last_idx = lines.len().saturating_sub(1);
            for (i, line) in lines.iter().enumerate() {
                if line.is_empty() {
                    continue;
                }
                match serde_json::from_str::<(String, Option<ContentDependentRecord>)>(line) {
                    Ok((rk, Some(r))) => {
                        index.insert(rk, r);
                    }
                    Ok((rk, None)) => {
                        index.remove(&rk);
                    }
                    // Same reasoning as `ShardMetaStore::open` (#152): only
                    // the last line can be a torn write from a crash
                    // mid-append, so quarantine just that one with a
                    // warning instead of refusing to start.
                    Err(e) if i == last_idx => {
                        log::warn!(
                            "content location log {}: ignoring malformed trailing line, \
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
            index: RwLock::new(index),
            write_lock: std::sync::Mutex::new(()),
        })
    }
}

impl ContentLocationStore for BitcaskContentLocationStore {
    fn put(&self, record: ContentDependentRecord) -> io::Result<()> {
        let rk = record_key(&record.user_id, &record.bucket, &record.key);
        let line = serde_json::to_string(&(rk.clone(), Some(record.clone())))
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        // #155: held across both the append and the index update, so a
        // concurrent put/delete for the same key can never apply to the
        // log and the index in different orders.
        let _write_guard = self.write_lock.lock().unwrap();
        {
            let mut f = self.log_file.write().unwrap();
            writeln!(f, "{line}")?;
            f.flush()?;
        }
        self.index.write().unwrap().insert(rk, record);
        Ok(())
    }

    fn get(&self, user_id: &str, bucket: &str, key: &str) -> Option<ContentDependentRecord> {
        self.index.read().unwrap().get(&record_key(user_id, bucket, key)).cloned()
    }

    fn delete(&self, user_id: &str, bucket: &str, key: &str) -> io::Result<()> {
        let rk = record_key(user_id, bucket, key);
        let line = serde_json::to_string(&(rk.clone(), Option::<ContentDependentRecord>::None))
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let _write_guard = self.write_lock.lock().unwrap();
        {
            let mut f = self.log_file.write().unwrap();
            writeln!(f, "{line}")?;
            f.flush()?;
        }
        self.index.write().unwrap().remove(&rk);
        Ok(())
    }

    fn list(&self, user_id: &str, bucket: &str, prefix: &str) -> Vec<(String, u64)> {
        let bucket_prefix = format!("{}:{}:{}:{}:", user_id.len(), user_id, bucket.len(), bucket);
        let full_prefix = format!("{bucket_prefix}{prefix}");
        self.index
            .read()
            .unwrap()
            .range(full_prefix.clone()..)
            .take_while(|(k, _)| k.starts_with(&full_prefix))
            .map(|(k, v)| (k[bucket_prefix.len()..].to_string(), v.original_len as u64))
            .collect()
    }

    fn compact(&self) -> io::Result<()> {
        // #155: same `write_lock` as `put`/`delete`, so compaction can't
        // interleave with either.
        let _write_guard = self.write_lock.lock().unwrap();
        let index = self.index.read().unwrap();
        let mut log_file = self.log_file.write().unwrap();
        let tmp_path = self.log_path.with_extension("compact.tmp");
        {
            let mut tmp = File::create(&tmp_path)?;
            for (rk, record) in index.iter() {
                let line = serde_json::to_string(&(rk.clone(), Some(record.clone())))
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                writeln!(tmp, "{line}")?;
            }
            tmp.flush()?;
        }
        std::fs::rename(&tmp_path, &self.log_path)?;
        *log_file = OpenOptions::new().create(true).append(true).open(&self.log_path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(bucket: &str, key: &str) -> ContentDependentRecord {
        sample_for("u1", bucket, key)
    }

    fn sample_for(user_id: &str, bucket: &str, key: &str) -> ContentDependentRecord {
        ContentDependentRecord {
            user_id: user_id.to_string(),
            bucket: bucket.to_string(),
            key: key.to_string(),
            k: 3,
            m: 2,
            original_len: 100,
            units: vec![
                UnitMeta { unit_id: "u0".into(), offset: 0, len: 50, uncompressed_len: 50, codec: "opaque".into(), metadata: Vec::new() },
                UnitMeta { unit_id: "u1".into(), offset: 50, len: 50, uncompressed_len: 50, codec: "opaque".into(), metadata: Vec::new() },
            ],
            stripes: vec![StripeRecord {
                shard_peers: vec!["http://node0:9710".into(), "http://node1:9710".into()],
                capacity: 50,
                bins: vec![vec!["u0".into()], vec!["u1".into()], vec![]],
            }],
            version: "v1".to_string(),
            has_sparse_gaps: false,
            sorted_unit_locations: Vec::new(),
            etag: None,
            content_type: None,
            last_modified: None,
            user_metadata: HashMap::new(),
            cache_control: None,
            expires: None,
            content_encoding: None,
            checksum_algorithm: None,
            checksum_value: None,
            checksum_type: None,
            tags: Vec::new(),
        }
    }

    fn loc(offset: u64, end: u64, stripe_index: usize, bin_idx: usize, pos_in_bin: usize) -> UnitLocation {
        UnitLocation { offset, end, stripe_index, bin_idx, pos_in_bin, len: (end - offset) as usize }
    }

    fn sorted(mut locs: Vec<UnitLocation>) -> Vec<UnitLocation> {
        locs.sort_by_key(|u| u.offset);
        locs
    }

    #[test]
    fn units_overlapping_finds_the_single_unit_a_selective_range_touches() {
        let mut record = sample("b", "k");
        record.sorted_unit_locations = sorted(vec![
            loc(0, 100, 0, 0, 0),
            loc(100, 200, 1, 0, 0),
            loc(200, 300, 2, 0, 0),
        ]);
        let got: Vec<usize> = record.units_overlapping(150, 150).iter().map(|u| u.stripe_index).collect();
        assert_eq!(got, vec![1]);
    }

    #[test]
    fn units_overlapping_spans_multiple_units_without_missing_boundaries() {
        let mut record = sample("b", "k");
        record.sorted_unit_locations = sorted(vec![
            loc(0, 10, 0, 0, 0),
            loc(10, 20, 1, 0, 0),
            loc(20, 30, 2, 0, 0),
        ]);
        // [5, 25] overlaps all three units.
        let got: Vec<usize> = record.units_overlapping(5, 25).iter().map(|u| u.stripe_index).collect();
        assert_eq!(got, vec![0, 1, 2]);
    }

    #[test]
    fn units_overlapping_exact_boundary_goes_to_the_right_unit_only() {
        let mut record = sample("b", "k");
        record.sorted_unit_locations = sorted(vec![loc(0, 10, 0, 0, 0), loc(10, 20, 1, 0, 0)]);
        // end is inclusive, unit end is exclusive: byte 10 belongs to unit 1.
        let got: Vec<usize> = record.units_overlapping(10, 10).iter().map(|u| u.stripe_index).collect();
        assert_eq!(got, vec![1]);
        let got: Vec<usize> = record.units_overlapping(9, 9).iter().map(|u| u.stripe_index).collect();
        assert_eq!(got, vec![0]);
    }

    #[test]
    fn units_overlapping_before_or_after_all_units_finds_nothing() {
        let mut record = sample("b", "k");
        record.sorted_unit_locations = sorted(vec![loc(100, 110, 0, 0, 0)]);
        assert!(record.units_overlapping(0, 50).is_empty());
        assert!(record.units_overlapping(200, 250).is_empty());
    }

    /// #150's exact repro, applied to this store's own index: bucket
    /// `a/b` + key `c` must not collide with bucket `a` + key `b/c`.
    #[test]
    fn record_key_does_not_collide_when_a_separator_moves_across_the_bucket_key_boundary() {
        assert_ne!(record_key("u1", "a/b", "c"), record_key("u1", "a", "b/c"));
    }

    /// Tenant-isolation fix: two different users writing the exact same
    /// `(bucket, key)` must not collide onto one record.
    #[test]
    fn record_key_does_not_collide_across_different_users_for_the_same_bucket_and_key() {
        assert_ne!(record_key("alice", "shared-bucket", "k"), record_key("bob", "shared-bucket", "k"));
    }

    #[test]
    fn two_users_writing_the_same_bucket_and_key_get_independent_records() {
        let dir = tempfile::tempdir().unwrap();
        let store = BitcaskContentLocationStore::open(dir.path().join("cloc.log")).unwrap();

        let mut alice_rec = sample_for("alice", "logs", "k1");
        alice_rec.original_len = 111;
        let mut bob_rec = sample_for("bob", "logs", "k1");
        bob_rec.original_len = 222;

        store.put(alice_rec.clone()).unwrap();
        store.put(bob_rec.clone()).unwrap();

        assert_eq!(store.get("alice", "logs", "k1").unwrap(), alice_rec);
        assert_eq!(store.get("bob", "logs", "k1").unwrap(), bob_rec);

        store.delete("alice", "logs", "k1").unwrap();
        assert!(store.get("alice", "logs", "k1").is_none());
        assert_eq!(store.get("bob", "logs", "k1").unwrap(), bob_rec, "deleting alice's record must not touch bob's");
    }

    #[test]
    fn put_then_get_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let store = BitcaskContentLocationStore::open(dir.path().join("cloc.log")).unwrap();
        store.put(sample("b1", "k1")).unwrap();
        assert_eq!(store.get("u1", "b1", "k1").unwrap(), sample("b1", "k1"));
    }

    #[test]
    fn restart_rebuilds_index_from_log_replay() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("cloc.log");
        {
            let store = BitcaskContentLocationStore::open(&log_path).unwrap();
            store.put(sample("b1", "k1")).unwrap();
        }
        let reopened = BitcaskContentLocationStore::open(&log_path).unwrap();
        assert_eq!(reopened.get("u1", "b1", "k1").unwrap(), sample("b1", "k1"));
    }

    #[test]
    fn delete_writes_a_tombstone() {
        let dir = tempfile::tempdir().unwrap();
        let store = BitcaskContentLocationStore::open(dir.path().join("cloc.log")).unwrap();
        store.put(sample("b1", "k1")).unwrap();
        store.delete("u1", "b1", "k1").unwrap();
        assert!(store.get("u1", "b1", "k1").is_none());
    }

    /// Same reasoning as `shard_meta_store.rs`'s matching test (#152): a
    /// torn last line (crash mid-`writeln!`) shouldn't take the whole
    /// store down on the next restart.
    #[test]
    fn a_malformed_trailing_line_is_quarantined_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("cloc.log");
        {
            let store = BitcaskContentLocationStore::open(&log_path).unwrap();
            store.put(sample("b1", "k1")).unwrap();
        }
        {
            let mut f = OpenOptions::new().append(true).open(&log_path).unwrap();
            writeln!(f, "[\"b1:k2\",{{\"bucket\":\"b1\"").unwrap();
        }
        let reopened = BitcaskContentLocationStore::open(&log_path).unwrap();
        assert_eq!(reopened.get("u1", "b1", "k1").unwrap(), sample("b1", "k1"));
        assert!(reopened.get("u1", "b1", "k2").is_none());
    }

    #[test]
    fn locate_unit_finds_its_stripe_and_bin() {
        let record = sample("b1", "k1");
        assert_eq!(record.locate_unit("u0"), Some((0, 0)));
        assert_eq!(record.locate_unit("u1"), Some((0, 1)));
        assert_eq!(record.locate_unit("not-a-real-unit"), None);
    }

    #[test]
    fn list_returns_only_keys_matching_the_prefix_in_this_bucket() {
        let dir = tempfile::tempdir().unwrap();
        let store = BitcaskContentLocationStore::open(dir.path().join("cloc.log")).unwrap();
        store.put(sample("b1", "reports/jan")).unwrap();
        store.put(sample("b1", "reports/feb")).unwrap();
        store.put(sample("b1", "other")).unwrap();
        // "b1z" sorts between "b1/reports/..." and "b1/reports0" in a plain
        // lexicographic BTreeMap over "bucket/key" strings — a range query
        // that used an unsound exclusive-upper-bound trick instead of
        // `take_while` could leak this in or cut off real matches early.
        store.put(sample("b1z", "reports/mar")).unwrap();

        let mut got = store.list("u1", "b1", "reports/");
        got.sort();
        assert_eq!(
            got,
            vec![("reports/feb".to_string(), 100), ("reports/jan".to_string(), 100)]
        );
    }

    #[test]
    fn compact_preserves_live_data_and_drops_dead_entries_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("cloc.log");
        let store = BitcaskContentLocationStore::open(&log_path).unwrap();
        store.put(sample("b1", "keep")).unwrap();
        for i in 0..20 {
            store.put(sample("b1", &format!("churn{i}"))).unwrap();
            store.delete("u1", "b1", &format!("churn{i}")).unwrap();
        }
        let size_before = std::fs::metadata(&log_path).unwrap().len();

        store.compact().unwrap();

        let size_after = std::fs::metadata(&log_path).unwrap().len();
        assert!(size_after < size_before, "compact should shrink the log file");

        assert_eq!(store.get("u1", "b1", "keep").unwrap(), sample("b1", "keep"));
        for i in 0..20 {
            assert!(store.get("u1", "b1", &format!("churn{i}")).is_none());
        }

        // Correctness survives a real reopen, not just the in-memory index.
        let reopened = BitcaskContentLocationStore::open(&log_path).unwrap();
        assert_eq!(reopened.get("u1", "b1", "keep").unwrap(), sample("b1", "keep"));
        for i in 0..20 {
            assert!(reopened.get("u1", "b1", &format!("churn{i}")).is_none());
        }
    }

    #[test]
    fn a_write_after_compaction_still_works() {
        let dir = tempfile::tempdir().unwrap();
        let store = BitcaskContentLocationStore::open(dir.path().join("cloc.log")).unwrap();
        store.put(sample("b1", "k1")).unwrap();
        store.compact().unwrap();
        store.put(sample("b1", "k2")).unwrap();
        assert_eq!(store.get("u1", "b1", "k1").unwrap(), sample("b1", "k1"));
        assert_eq!(store.get("u1", "b1", "k2").unwrap(), sample("b1", "k2"));
    }

    /// Same invariant and same reasoning as `location_store.rs`'s matching
    /// test (#155): the live index and a fresh replay of the log must
    /// always agree, however a concurrent put/delete race on the same key
    /// actually resolved.
    #[test]
    fn concurrent_put_delete_on_the_same_key_never_diverges_log_from_index() {
        use std::thread;

        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("cloc.log");
        let store = std::sync::Arc::new(BitcaskContentLocationStore::open(&log_path).unwrap());

        let handles: Vec<_> = (0..16)
            .map(|t| {
                let store = std::sync::Arc::clone(&store);
                thread::spawn(move || {
                    for i in 0..50 {
                        if (t + i) % 2 == 0 {
                            store.put(sample("b1", "racykey")).unwrap();
                        } else {
                            let _ = store.delete("u1", "b1", "racykey");
                        }
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let live = store.get("u1", "b1", "racykey");
        let replayed = BitcaskContentLocationStore::open(&log_path).unwrap().get("u1", "b1", "racykey");
        assert_eq!(live, replayed, "live index and a fresh log replay must agree");
    }
}
