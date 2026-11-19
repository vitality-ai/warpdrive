//! Per-node key-value engine under the cluster's record stores (RFC 5,
//! migration step 1).
//!
//! `LocationStore`, `ContentLocationStore` and `ShardMetaStore` each keep
//! one node's share of cluster metadata. What they store, and how the
//! cluster replicates it, is their business. How the bytes sit on this
//! node's disk is the engine's. Splitting the two means another engine
//! (RocksDB, SQLite, ...) is one `KvEngine` implementation, with no store
//! or replication code in it.
//!
//! Stores are generic over the engine, not `dyn`, so the default engine
//! costs nothing over the code it replaced: every call is a direct,
//! inlinable call.
//!
//! The default engine, `BitcaskEngine`, is the format the stores already
//! used: an append-only JSON-lines log of `(key, Option<value>)`, where
//! `None` is a delete, and a full in-memory index rebuilt by replaying the
//! log at startup.

use parking_lot::{Mutex, RwLock};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use super::durable_log::DurableAppendLog;

/// One node's durable key-value storage.
///
/// Writes return once they're durable. Reads see every write that has
/// returned, and may see a write that is still waiting on its sync.
pub trait KvEngine<V>: Send + Sync {
    fn get(&self, key: &str) -> Option<V>;
    /// Durably sets `key` to `value`.
    fn put(&self, key: String, value: V) -> io::Result<()>;
    /// Durably removes `key`. Removing a missing key is not an error.
    fn delete(&self, key: &str) -> io::Result<()>;
    /// Durably sets `key` to `value` only if `check`, given the current
    /// value, returns `Ok`. The check and the write are atomic with respect
    /// to every other write to the engine. Returns the value it replaced,
    /// or `check`'s error without writing anything.
    fn put_if<T>(&self, key: String, value: V, check: impl FnOnce(Option<&V>) -> Result<(), T>) -> io::Result<Result<Option<V>, T>>;
    /// `f` applied to every entry whose key starts with `prefix`, in key
    /// order, keeping the `Some` results.
    fn scan_prefix<T>(&self, prefix: &str, f: impl FnMut(&str, &V) -> Option<T>) -> Vec<T>;
    /// The entry with the greatest key that is at most `key` and starts
    /// with `prefix`.
    fn floor(&self, prefix: &str, key: &str) -> Option<(String, V)>;
    /// Every entry, as owned pairs.
    fn snapshot(&self) -> Vec<(String, V)>;
    /// Reclaims space taken by overwritten and deleted entries.
    fn compact(&self) -> io::Result<()>;
}

/// The in-memory index a `BitcaskEngine` keeps. `BTreeMap` makes prefix
/// scans a bounded range walk. `HashMap` is faster for stores that only
/// do point lookups.
pub trait Index<V>: Default + Send + Sync {
    fn get(&self, key: &str) -> Option<&V>;
    /// Returns the value it replaced.
    fn insert(&mut self, key: String, value: V) -> Option<V>;
    fn remove(&mut self, key: &str);
    fn len(&self) -> usize;
    fn for_each(&self, f: impl FnMut(&String, &V));
    fn scan_prefix<T>(&self, prefix: &str, f: impl FnMut(&str, &V) -> Option<T>) -> Vec<T>;
    fn floor(&self, prefix: &str, key: &str) -> Option<(&String, &V)>;
}

impl<V: Send + Sync> Index<V> for BTreeMap<String, V> {
    fn get(&self, key: &str) -> Option<&V> {
        BTreeMap::get(self, key)
    }
    fn insert(&mut self, key: String, value: V) -> Option<V> {
        BTreeMap::insert(self, key, value)
    }
    fn remove(&mut self, key: &str) {
        BTreeMap::remove(self, key);
    }
    fn len(&self) -> usize {
        BTreeMap::len(self)
    }
    fn for_each(&self, mut f: impl FnMut(&String, &V)) {
        self.iter().for_each(|(k, v)| f(k, v));
    }
    fn scan_prefix<T>(&self, prefix: &str, mut f: impl FnMut(&str, &V) -> Option<T>) -> Vec<T> {
        // A range starting at the prefix, taken while keys still match it:
        // O(log n + matches), not a full walk (#163).
        self.range::<str, _>((std::ops::Bound::Included(prefix), std::ops::Bound::Unbounded))
            .take_while(|(k, _)| k.starts_with(prefix))
            .filter_map(|(k, v)| f(k, v))
            .collect()
    }
    fn floor(&self, prefix: &str, key: &str) -> Option<(&String, &V)> {
        self.range::<str, _>((std::ops::Bound::Unbounded, std::ops::Bound::Included(key)))
            .next_back()
            .filter(|(k, _)| k.starts_with(prefix))
    }
}

impl<V: Send + Sync> Index<V> for HashMap<String, V> {
    fn get(&self, key: &str) -> Option<&V> {
        HashMap::get(self, key)
    }
    fn insert(&mut self, key: String, value: V) -> Option<V> {
        HashMap::insert(self, key, value)
    }
    fn remove(&mut self, key: &str) {
        HashMap::remove(self, key);
    }
    fn len(&self) -> usize {
        HashMap::len(self)
    }
    fn for_each(&self, mut f: impl FnMut(&String, &V)) {
        self.iter().for_each(|(k, v)| f(k, v));
    }
    fn scan_prefix<T>(&self, prefix: &str, mut f: impl FnMut(&str, &V) -> Option<T>) -> Vec<T> {
        let mut hits: Vec<(&String, &V)> = self.iter().filter(|(k, _)| k.starts_with(prefix)).collect();
        hits.sort_by(|a, b| a.0.cmp(b.0));
        hits.into_iter().filter_map(|(k, v)| f(k, v)).collect()
    }
    fn floor(&self, prefix: &str, key: &str) -> Option<(&String, &V)> {
        self.iter().filter(|(k, _)| k.starts_with(prefix) && k.as_str() <= key).max_by(|a, b| a.0.cmp(b.0))
    }
}

/// Append-only log plus a full in-memory index.
pub struct BitcaskEngine<V, I = BTreeMap<String, V>> {
    log_path: PathBuf,
    log: DurableAppendLog,
    index: RwLock<I>,
    /// Held across the log append and the index update (#155), so the log
    /// and the index always apply writes in the same order. Released before
    /// the durability wait, so group commit can batch many writers' syncs.
    write_lock: Mutex<()>,
    label: &'static str,
    _value: std::marker::PhantomData<fn() -> V>,
}

impl<V, I> BitcaskEngine<V, I>
where
    V: Serialize + DeserializeOwned + Clone + Send + Sync,
    I: Index<V>,
{
    /// Opens (creating if needed) the log at `log_path` and replays it.
    /// `label` names the store in log messages.
    pub fn open(log_path: impl AsRef<Path>, label: &'static str) -> io::Result<Self> {
        let log_path = log_path.as_ref().to_path_buf();
        if let Some(parent) = log_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut index = I::default();
        if log_path.exists() {
            let f = File::open(&log_path)?;
            let lines: Vec<String> = BufReader::new(f).lines().collect::<io::Result<_>>()?;
            let last_idx = lines.len().saturating_sub(1);
            for (i, line) in lines.iter().enumerate() {
                if line.is_empty() {
                    continue;
                }
                match serde_json::from_str::<(String, Option<V>)>(line) {
                    Ok((key, Some(v))) => {
                        index.insert(key, v);
                    }
                    Ok((key, None)) => index.remove(&key),
                    // Only the last line can be a torn write from a crash
                    // mid-append (#152), so only that one is skipped. A
                    // malformed line anywhere else is corruption.
                    Err(e) if i == last_idx => {
                        log::warn!(
                            "{label} log {}: ignoring malformed trailing line, likely a torn write from a crash: {e}",
                            log_path.display()
                        );
                    }
                    Err(e) => return Err(io::Error::new(io::ErrorKind::InvalidData, e)),
                }
            }
        }

        let log = DurableAppendLog::open(&log_path)?;
        Ok(Self {
            log_path,
            log,
            index: RwLock::new(index),
            write_lock: Mutex::new(()),
            label,
            _value: std::marker::PhantomData,
        })
    }

    pub fn path(&self) -> &Path {
        &self.log_path
    }

    fn line(key: &str, value: Option<&V>) -> io::Result<String> {
        serde_json::to_string(&(key, value)).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
}

impl<V, I> KvEngine<V> for BitcaskEngine<V, I>
where
    V: Serialize + DeserializeOwned + Clone + Send + Sync,
    I: Index<V>,
{
    fn get(&self, key: &str) -> Option<V> {
        self.index.read().get(key).cloned()
    }

    fn put(&self, key: String, value: V) -> io::Result<()> {
        // Serialized before taking the lock, so the lock covers only the
        // append and the index update.
        let line = Self::line(&key, Some(&value))?;
        let ticket = {
            let _write_guard = self.write_lock.lock();
            let ticket = self.log.append_line(&line)?;
            self.index.write().insert(key, value);
            ticket
        };
        self.log.sync_ticket(ticket)
    }

    fn put_if<T>(&self, key: String, value: V, check: impl FnOnce(Option<&V>) -> Result<(), T>) -> io::Result<Result<Option<V>, T>> {
        // Serialized before taking the lock, as in `put`. A refused write
        // wastes the serialization, which is cheap next to the sync.
        let line = Self::line(&key, Some(&value))?;
        let (ticket, previous) = {
            let _write_guard = self.write_lock.lock();
            // Writers are serialized by `write_lock`, so the value checked
            // here is still current when the index is updated below.
            if let Err(e) = check(self.index.read().get(&key)) {
                return Ok(Err(e));
            }
            let ticket = self.log.append_line(&line)?;
            let previous = self.index.write().insert(key, value);
            (ticket, previous)
        };
        self.log.sync_ticket(ticket)?;
        Ok(Ok(previous))
    }

    fn delete(&self, key: &str) -> io::Result<()> {
        let line = Self::line(key, None)?;
        let ticket = {
            let _write_guard = self.write_lock.lock();
            let ticket = self.log.append_line(&line)?;
            self.index.write().remove(key);
            ticket
        };
        self.log.sync_ticket(ticket)
    }

    fn scan_prefix<T>(&self, prefix: &str, f: impl FnMut(&str, &V) -> Option<T>) -> Vec<T> {
        self.index.read().scan_prefix(prefix, f)
    }

    fn floor(&self, prefix: &str, key: &str) -> Option<(String, V)> {
        self.index.read().floor(prefix, key).map(|(k, v)| (k.clone(), v.clone()))
    }

    fn snapshot(&self) -> Vec<(String, V)> {
        let index = self.index.read();
        let mut out = Vec::with_capacity(index.len());
        index.for_each(|k, v| out.push((k.clone(), v.clone())));
        out
    }

    /// Rewrites the log to one line per live entry, then atomically
    /// replaces it (#163). Holds the write lock for the rewrite.
    fn compact(&self) -> io::Result<()> {
        let _write_guard = self.write_lock.lock();
        let index = self.index.read();
        let tmp_path = self.log_path.with_extension("compact.tmp");
        {
            let mut tmp = io::BufWriter::new(File::create(&tmp_path)?);
            let mut err = None;
            index.for_each(|k, v| {
                if err.is_none() {
                    if let Err(e) = Self::line(k, Some(v)).and_then(|l| writeln!(tmp, "{l}")) {
                        err = Some(e);
                    }
                }
            });
            if let Some(e) = err {
                return Err(e);
            }
            tmp.flush()?;
        }
        log::debug!("{} log {} compacted to {} entries", self.label, self.log_path.display(), index.len());
        self.log.replace_with_compacted(&tmp_path, &self.log_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;

    type Engine = BitcaskEngine<u64>;
    type HashEngine = BitcaskEngine<u64, HashMap<String, u64>>;

    fn open(dir: &tempfile::TempDir) -> Engine {
        Engine::open(dir.path().join("e.log"), "test").unwrap()
    }

    #[test]
    fn put_get_delete_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        {
            let e = open(&dir);
            e.put("a".into(), 1).unwrap();
            e.put("b".into(), 2).unwrap();
            e.put("a".into(), 3).unwrap();
            e.delete("b").unwrap();
            e.delete("never-existed").unwrap();
        }
        let e = open(&dir);
        assert_eq!((e.get("a"), e.get("b")), (Some(3), None));
    }

    #[test]
    fn the_log_format_is_unchanged() {
        // The stores wrote `(key, Option<value>)` JSON lines before the
        // engine existed. Existing logs must still replay.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("e.log");
        {
            let mut f = OpenOptions::new().create(true).append(true).open(&path).unwrap();
            writeln!(f, "[\"k1\",7]").unwrap();
            writeln!(f, "[\"k2\",8]").unwrap();
            writeln!(f, "[\"k1\",null]").unwrap();
        }
        let e = Engine::open(&path, "test").unwrap();
        assert_eq!((e.get("k1"), e.get("k2")), (None, Some(8)));
        e.put("k3".into(), 9).unwrap();
        let last = std::fs::read_to_string(&path).unwrap().lines().last().unwrap().to_string();
        assert_eq!(last, "[\"k3\",9]");
    }

    #[test]
    fn a_torn_last_line_is_skipped_but_corruption_elsewhere_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("e.log");
        std::fs::write(&path, "[\"k1\",7]\n[\"k2\",8").unwrap();
        assert_eq!(Engine::open(&path, "test").unwrap().get("k1"), Some(7));
        std::fs::write(&path, "garbage\n[\"k1\",7]\n").unwrap();
        assert!(Engine::open(&path, "test").is_err());
    }

    #[test]
    fn prefix_scans_return_only_matching_keys_in_order_for_both_indexes() {
        let dir = tempfile::tempdir().unwrap();
        let b = open(&dir);
        let h = HashEngine::open(dir.path().join("h.log"), "test").unwrap();
        for (k, v) in [("p/b", 2), ("p/a", 1), ("pz", 9), ("o/x", 0)] {
            b.put(k.into(), v).unwrap();
            h.put(k.into(), v).unwrap();
        }
        let want = vec![("p/a".to_string(), 1), ("p/b".to_string(), 2)];
        assert_eq!(b.scan_prefix("p/", |k, v| Some((k.to_string(), *v))), want);
        assert_eq!(h.scan_prefix("p/", |k, v| Some((k.to_string(), *v))), want);
    }

    #[test]
    fn put_if_writes_only_when_the_check_passes_and_returns_what_it_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let e = open(&dir);
        assert_eq!(e.put_if("k".into(), 5, |cur| if cur.is_none() { Ok(()) } else { Err("exists") }).unwrap(), Ok(None));
        assert_eq!(e.put_if("k".into(), 6, |cur| if cur.is_none() { Ok(()) } else { Err("exists") }).unwrap(), Err("exists"));
        assert_eq!(e.put_if("k".into(), 7, |cur| if cur == Some(&5) { Ok(()) } else { Err("changed") }).unwrap(), Ok(Some(5)));
        assert_eq!(open(&dir).get("k"), Some(7), "a refused write left nothing in the log");
    }

    #[test]
    fn floor_finds_the_nearest_key_at_or_below_within_the_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let e = open(&dir);
        for (k, v) in [("a/10", 1), ("a/20", 2), ("b/05", 3)] {
            e.put(k.into(), v).unwrap();
        }
        assert_eq!(e.floor("a/", "a/15"), Some(("a/10".into(), 1)));
        assert_eq!(e.floor("a/", "a/20"), Some(("a/20".into(), 2)));
        assert_eq!(e.floor("a/", "a/99"), Some(("a/20".into(), 2)));
        assert_eq!(e.floor("a/", "a/05"), None);
        assert_eq!(e.floor("b/", "b/04"), None, "a key in another prefix isn't returned");
    }

    #[test]
    fn compaction_shrinks_the_log_and_keeps_every_live_entry() {
        let dir = tempfile::tempdir().unwrap();
        let e = open(&dir);
        e.put("keep".into(), 1).unwrap();
        for i in 0..50 {
            e.put(format!("churn{i}"), i).unwrap();
            e.delete(&format!("churn{i}")).unwrap();
        }
        let before = std::fs::metadata(e.path()).unwrap().len();
        e.compact().unwrap();
        assert!(std::fs::metadata(e.path()).unwrap().len() < before);
        e.put("after".into(), 2).unwrap();
        let e = open(&dir);
        assert_eq!((e.get("keep"), e.get("after"), e.get("churn3")), (Some(1), Some(2), None));
    }

    /// #155: the live index and a fresh replay must agree, however
    /// concurrent writers to one key interleave.
    #[test]
    fn concurrent_writers_never_leave_the_log_and_index_disagreeing() {
        let dir = tempfile::tempdir().unwrap();
        let e = std::sync::Arc::new(open(&dir));
        let handles: Vec<_> = (0..16u64)
            .map(|t| {
                let e = std::sync::Arc::clone(&e);
                std::thread::spawn(move || {
                    for i in 0..50u64 {
                        if (t + i) % 2 == 0 {
                            e.put("k".into(), t * 1000 + i).unwrap();
                        } else {
                            e.delete("k").unwrap();
                        }
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(e.get("k"), open(&dir).get("k"));
    }
}
