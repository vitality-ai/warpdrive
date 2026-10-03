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
use std::collections::BTreeMap;
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ContentDependentRecord {
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
    fn get(&self, bucket: &str, key: &str) -> Option<ContentDependentRecord>;
    fn delete(&self, bucket: &str, key: &str) -> io::Result<()>;
    /// See `LocationStore::list` — same minimal-S3-surface purpose, for the
    /// content-dependent half of a bucket's objects.
    fn list(&self, bucket: &str, prefix: &str) -> Vec<(String, u64)>;
    /// See `LocationStore::compact` — same reasoning and same mechanism
    /// (#163), applied to this store's own log.
    fn compact(&self) -> io::Result<()>;
}

/// Length-prefixed, same scheme and same reasoning as `location_store.rs`'s
/// `record_key` (#150: a plain `/` separator lets bucket `a/b` + key `c`
/// and bucket `a` + key `b/c` collide onto the same index entry).
fn record_key(bucket: &str, key: &str) -> String {
    format!("{}:{}:{}", bucket.len(), bucket, key)
}

/// `BTreeMap`, not `HashMap` (#163) — see `location_store.rs`'s matching
/// doc comment, same reasoning: `record_key`'s length-prefixed bucket
/// still groups each bucket's keys contiguously, so lexicographic order
/// makes `list`'s prefix lookup a bounded range scan.
pub struct BitcaskContentLocationStore {
    log_path: PathBuf,
    log_file: RwLock<File>,
    index: RwLock<BTreeMap<String, ContentDependentRecord>>,
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
        })
    }
}

impl ContentLocationStore for BitcaskContentLocationStore {
    fn put(&self, record: ContentDependentRecord) -> io::Result<()> {
        let rk = record_key(&record.bucket, &record.key);
        let line = serde_json::to_string(&(rk.clone(), Some(record.clone())))
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        {
            let mut f = self.log_file.write().unwrap();
            writeln!(f, "{line}")?;
            f.flush()?;
        }
        self.index.write().unwrap().insert(rk, record);
        Ok(())
    }

    fn get(&self, bucket: &str, key: &str) -> Option<ContentDependentRecord> {
        self.index.read().unwrap().get(&record_key(bucket, key)).cloned()
    }

    fn delete(&self, bucket: &str, key: &str) -> io::Result<()> {
        let rk = record_key(bucket, key);
        let line = serde_json::to_string(&(rk.clone(), Option::<ContentDependentRecord>::None))
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        {
            let mut f = self.log_file.write().unwrap();
            writeln!(f, "{line}")?;
            f.flush()?;
        }
        self.index.write().unwrap().remove(&rk);
        Ok(())
    }

    fn list(&self, bucket: &str, prefix: &str) -> Vec<(String, u64)> {
        let bucket_prefix = format!("{}:{}:", bucket.len(), bucket);
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
        ContentDependentRecord {
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
        }
    }

    /// #150's exact repro, applied to this store's own index: bucket
    /// `a/b` + key `c` must not collide with bucket `a` + key `b/c`.
    #[test]
    fn record_key_does_not_collide_when_a_separator_moves_across_the_bucket_key_boundary() {
        assert_ne!(record_key("a/b", "c"), record_key("a", "b/c"));
    }

    #[test]
    fn put_then_get_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let store = BitcaskContentLocationStore::open(dir.path().join("cloc.log")).unwrap();
        store.put(sample("b1", "k1")).unwrap();
        assert_eq!(store.get("b1", "k1").unwrap(), sample("b1", "k1"));
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
        assert_eq!(reopened.get("b1", "k1").unwrap(), sample("b1", "k1"));
    }

    #[test]
    fn delete_writes_a_tombstone() {
        let dir = tempfile::tempdir().unwrap();
        let store = BitcaskContentLocationStore::open(dir.path().join("cloc.log")).unwrap();
        store.put(sample("b1", "k1")).unwrap();
        store.delete("b1", "k1").unwrap();
        assert!(store.get("b1", "k1").is_none());
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
        assert_eq!(reopened.get("b1", "k1").unwrap(), sample("b1", "k1"));
        assert!(reopened.get("b1", "k2").is_none());
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

        let mut got = store.list("b1", "reports/");
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
            store.delete("b1", &format!("churn{i}")).unwrap();
        }
        let size_before = std::fs::metadata(&log_path).unwrap().len();

        store.compact().unwrap();

        let size_after = std::fs::metadata(&log_path).unwrap().len();
        assert!(size_after < size_before, "compact should shrink the log file");

        assert_eq!(store.get("b1", "keep").unwrap(), sample("b1", "keep"));
        for i in 0..20 {
            assert!(store.get("b1", &format!("churn{i}")).is_none());
        }

        // Correctness survives a real reopen, not just the in-memory index.
        let reopened = BitcaskContentLocationStore::open(&log_path).unwrap();
        assert_eq!(reopened.get("b1", "keep").unwrap(), sample("b1", "keep"));
        for i in 0..20 {
            assert!(reopened.get("b1", &format!("churn{i}")).is_none());
        }
    }

    #[test]
    fn a_write_after_compaction_still_works() {
        let dir = tempfile::tempdir().unwrap();
        let store = BitcaskContentLocationStore::open(dir.path().join("cloc.log")).unwrap();
        store.put(sample("b1", "k1")).unwrap();
        store.compact().unwrap();
        store.put(sample("b1", "k2")).unwrap();
        assert_eq!(store.get("b1", "k1").unwrap(), sample("b1", "k1"));
        assert_eq!(store.get("b1", "k2").unwrap(), sample("b1", "k2"));
    }
}
