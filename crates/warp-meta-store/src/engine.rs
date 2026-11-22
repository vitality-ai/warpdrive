//! Per-node key-value engine under the cluster's record stores (RFC 5,
//! migration step 1).
//!
//! `LocationStore`, `ContentLocationStore` and `ShardMetaStore` each keep
//! one node's share of cluster metadata. What they store, and how the
//! cluster replicates it, is their business. How the bytes sit on this
//! node's disk is the engine's. Splitting the two means another engine
//! (RocksDB, SQLite, ...) is one `Engine` implementation, with no store
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
use std::sync::atomic::{AtomicU64, Ordering};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use crate::durable_log::DurableAppendLog;

/// One node's durable key-value storage.
///
/// Writes return once they're durable. Reads see every write that has
/// returned, and may see a write that is still waiting on its sync.
pub trait Engine<V>: Send + Sync {
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
    /// Durably removes `key` only if `keep` returns false for its current
    /// value, atomically with respect to every other write. Returns whether
    /// it removed anything. A missing key is left alone.
    fn delete_if(&self, key: &str, keep: impl FnOnce(&V) -> bool) -> io::Result<bool>;
    /// `delete_if` for many keys, durable on return. An engine can make the
    /// whole batch durable at once instead of one key at a time. Returns
    /// how many it removed.
    fn delete_many_if(&self, keys: &[String], keep: impl Fn(&str, &V) -> bool) -> io::Result<usize> {
        let mut removed = 0;
        for k in keys {
            removed += self.delete_if(k, |v| keep(k, v))? as usize;
        }
        Ok(removed)
    }
    /// `f` applied to every entry whose key starts with `prefix`, in key
    /// order, keeping the `Some` results.
    fn scan_prefix<T>(&self, prefix: &str, f: impl FnMut(&str, &V) -> Option<T>) -> Vec<T>;
    /// The entry with the greatest key that is at most `key`, starts with
    /// `prefix`, and whose value `accept` takes.
    fn floor(&self, prefix: &str, key: &str, accept: impl Fn(&V) -> bool) -> Option<(String, V)>;
    /// Every entry, as owned pairs.
    fn snapshot(&self) -> Vec<(String, V)>;
    /// Reclaims space taken by overwritten and deleted entries.
    fn compact(&self) -> io::Result<()>;
    /// Whether enough space has built up that `compact` is worth running.
    /// Engines that reclaim on their own answer `false`.
    fn compaction_due(&self) -> bool {
        false
    }
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
    fn floor(&self, prefix: &str, key: &str, accept: impl Fn(&V) -> bool) -> Option<(&String, &V)>;
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
    fn floor(&self, prefix: &str, key: &str, accept: impl Fn(&V) -> bool) -> Option<(&String, &V)> {
        // No key is both at least `prefix` and at most a smaller `key`.
        // `range` panics on that empty range, so answer it here.
        if key < prefix {
            return None;
        }
        // Keys starting with `prefix` form one run that begins at `prefix`.
        // When `key` is past that run, every key in it is below `key`: take
        // the last accepted one. Otherwise walk back from `key`.
        if !key.starts_with(prefix) {
            return self
                .range::<str, _>((std::ops::Bound::Included(prefix), std::ops::Bound::Unbounded))
                .take_while(|(k, _)| k.starts_with(prefix))
                .filter(|(_, v)| accept(v))
                .last();
        }
        self.range::<str, _>((std::ops::Bound::Included(prefix), std::ops::Bound::Included(key)))
            .rev()
            .take_while(|(k, _)| k.starts_with(prefix))
            .find(|(_, v)| accept(v))
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
    fn floor(&self, prefix: &str, key: &str, accept: impl Fn(&V) -> bool) -> Option<(&String, &V)> {
        self.iter().filter(|(k, v)| k.starts_with(prefix) && k.as_str() <= key && accept(v)).max_by(|a, b| a.0.cmp(b.0))
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
    /// Lines in the log file, live or not. With the index size, tells when
    /// the log is worth compacting.
    lines: AtomicU64,
    /// One compaction at a time.
    compacting: Mutex<()>,
    label: &'static str,
    _value: std::marker::PhantomData<fn() -> V>,
}

/// A log is compacted once it holds more than this many lines per live
/// entry (RFC 6: "more than twice the size of its live entries").
const COMPACT_RATIO: u64 = 2;
/// ...and at least this many lines, so small logs aren't rewritten often.
const COMPACT_MIN_LINES: u64 = 10_000;
/// Entries copied per read-lock hold while compacting.
const COMPACT_CHUNK: usize = 4096;

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
        let mut lines_read = 0u64;
        if log_path.exists() {
            let f = File::open(&log_path)?;
            let lines: Vec<String> = BufReader::new(f).lines().collect::<io::Result<_>>()?;
            let last_idx = lines.len().saturating_sub(1);
            for (i, line) in lines.iter().enumerate() {
                if line.is_empty() {
                    continue;
                }
                lines_read += 1;
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
            lines: AtomicU64::new(lines_read),
            compacting: Mutex::new(()),
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

impl<V, I> Engine<V> for BitcaskEngine<V, I>
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
            self.lines.fetch_add(1, Ordering::Relaxed);
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
            self.lines.fetch_add(1, Ordering::Relaxed);
            let previous = self.index.write().insert(key, value);
            (ticket, previous)
        };
        self.log.sync_ticket(ticket)?;
        Ok(Ok(previous))
    }

    fn delete_if(&self, key: &str, keep: impl FnOnce(&V) -> bool) -> io::Result<bool> {
        let line = Self::line(key, None)?;
        let ticket = {
            let _write_guard = self.write_lock.lock();
            match self.index.read().get(key) {
                Some(v) if !keep(v) => {}
                _ => return Ok(false),
            }
            let ticket = self.log.append_line(&line)?;
            self.lines.fetch_add(1, Ordering::Relaxed);
            self.index.write().remove(key);
            ticket
        };
        self.log.sync_ticket(ticket)?;
        Ok(true)
    }

    fn delete_many_if(&self, keys: &[String], keep: impl Fn(&str, &V) -> bool) -> io::Result<usize> {
        // Appended in chunks under the write lock, so writers wait at most
        // one chunk, then made durable by one sync: a sync covers every
        // line appended before its ticket.
        let mut removed = 0;
        let mut last = None;
        for chunk in keys.chunks(4096) {
            let _write_guard = self.write_lock.lock();
            for key in chunk {
                match self.index.read().get(key) {
                    Some(v) if !keep(key, v) => {}
                    _ => continue,
                }
                last = Some(self.log.append_line(&Self::line(key, None)?)?);
                self.lines.fetch_add(1, Ordering::Relaxed);
                self.index.write().remove(key);
                removed += 1;
            }
        }
        if let Some(ticket) = last {
            self.log.sync_ticket(ticket)?;
        }
        Ok(removed)
    }

    fn delete(&self, key: &str) -> io::Result<()> {
        let line = Self::line(key, None)?;
        let ticket = {
            let _write_guard = self.write_lock.lock();
            let ticket = self.log.append_line(&line)?;
            self.lines.fetch_add(1, Ordering::Relaxed);
            self.index.write().remove(key);
            ticket
        };
        self.log.sync_ticket(ticket)
    }

    fn scan_prefix<T>(&self, prefix: &str, f: impl FnMut(&str, &V) -> Option<T>) -> Vec<T> {
        self.index.read().scan_prefix(prefix, f)
    }

    fn floor(&self, prefix: &str, key: &str, accept: impl Fn(&V) -> bool) -> Option<(String, V)> {
        self.index.read().floor(prefix, key, accept).map(|(k, v)| (k.clone(), v.clone()))
    }

    fn snapshot(&self) -> Vec<(String, V)> {
        let index = self.index.read();
        let mut out = Vec::with_capacity(index.len());
        index.for_each(|k, v| out.push((k.clone(), v.clone())));
        out
    }

    /// Rewrites the log to one line per live entry and atomically replaces
    /// it (#163), without stalling writers (RFC 6, section 8):
    ///
    /// 1. **Cut**, under the write lock for an instant: note the log's
    ///    length. Every line before it is already in the index.
    /// 2. **Snapshot**, without the write lock: every live entry, copied out
    ///    of the index in chunks under brief read locks, to a temp file,
    ///    synced. Writers keep appending past the cut meanwhile.
    /// 3. **Tail and swap**, under the write lock: append the lines written
    ///    since the cut, sync, and rename the temp file over the log.
    ///
    /// Replaying the result gives the index: a key changed after the cut is
    /// in the tail, which replays after the snapshot and wins. A write
    /// waiting on its sync at the swap is in the temp file, which is synced
    /// before the rename.
    fn compact(&self) -> io::Result<()> {
        let Some(_one_at_a_time) = self.compacting.try_lock() else { return Ok(()) };
        let cut = {
            let _write_guard = self.write_lock.lock();
            std::fs::metadata(&self.log_path)?.len()
        };

        let tmp_path = self.log_path.with_extension("compact.tmp");
        let mut tmp = io::BufWriter::new(File::create(&tmp_path)?);
        let mut keys = Vec::new();
        self.index.read().for_each(|k, _| keys.push(k.clone()));
        let mut written = 0u64;
        for chunk in keys.chunks(COMPACT_CHUNK) {
            let lines: Vec<String> = {
                let index = self.index.read();
                chunk.iter().filter_map(|k| index.get(k).map(|v| Self::line(k, Some(v)))).collect::<io::Result<_>>()?
            };
            for l in &lines {
                writeln!(tmp, "{l}")?;
            }
            written += lines.len() as u64;
        }
        tmp.flush()?;
        tmp.get_ref().sync_data()?;

        let _write_guard = self.write_lock.lock();
        let mut tail = Vec::new();
        {
            use std::io::{Read, Seek, SeekFrom};
            let mut f = File::open(&self.log_path)?;
            f.seek(SeekFrom::Start(cut))?;
            f.read_to_end(&mut tail)?;
        }
        let tail_lines = tail.iter().filter(|b| **b == b'\n').count() as u64;
        tmp.write_all(&tail)?;
        tmp.flush()?;
        drop(tmp);
        self.log.replace_with_compacted(&tmp_path, &self.log_path)?;
        self.lines.store(written + tail_lines, Ordering::Relaxed);
        log::debug!("{} log {} compacted: {written} live entries, {tail_lines} lines written during the compaction", self.label, self.log_path.display());
        Ok(())
    }

    fn compaction_due(&self) -> bool {
        let lines = self.lines.load(Ordering::Relaxed);
        lines >= COMPACT_MIN_LINES && lines > COMPACT_RATIO * self.index.read().len() as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;

    type TestEngine = BitcaskEngine<u64>;
    type HashEngine = BitcaskEngine<u64, HashMap<String, u64>>;

    fn open(dir: &tempfile::TempDir) -> TestEngine {
        TestEngine::open(dir.path().join("e.log"), "test").unwrap()
    }

    #[test]
    fn compaction_is_due_only_when_the_log_is_mostly_dead() {
        let dir = tempfile::tempdir().unwrap();
        let e = open(&dir);
        for i in 0..COMPACT_MIN_LINES {
            e.put("k".into(), i).unwrap();
        }
        assert!(e.compaction_due(), "10k lines for 1 live entry");
        e.compact().unwrap();
        assert!(!e.compaction_due(), "one line after compacting");
        let fresh = TestEngine::open(dir.path().join("e.log"), "test").unwrap();
        assert_eq!(fresh.get("k"), Some(COMPACT_MIN_LINES - 1));
        assert!(!fresh.compaction_due(), "the line count is rebuilt on open");
    }

    /// Writers run during a compaction: none waits for the snapshot, and
    /// the compacted log replays to exactly the index's final state.
    #[test]
    fn compaction_does_not_stall_writers_and_keeps_every_write() {
        let dir = tempfile::tempdir().unwrap();
        let e = std::sync::Arc::new(open(&dir));
        // Prefilled from 8 threads so group commit batches the syncs.
        std::thread::scope(|sc| {
            for t in 0..8u64 {
                let e = &e;
                sc.spawn(move || {
                    for i in (t..40_000u64).step_by(8) {
                        e.put(format!("k{}", i % 20_000), i).unwrap();
                    }
                });
            }
        });
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let writers: Vec<_> = (0..4)
            .map(|t| {
                let (e, done) = (e.clone(), done.clone());
                std::thread::spawn(move || {
                    let mut worst = std::time::Duration::ZERO;
                    let mut n = 0u64;
                    while !done.load(std::sync::atomic::Ordering::Relaxed) || n < 200 {
                        let t0 = std::time::Instant::now();
                        let key = format!("k{}", (t * 7919 + n) % 25_000);
                        if n % 5 == 0 { e.delete(&key).unwrap() } else { e.put(key, 1_000_000 + n).unwrap() }
                        worst = worst.max(t0.elapsed());
                        n += 1;
                    }
                    worst
                })
            })
            .collect();
        let t0 = std::time::Instant::now();
        e.compact().unwrap();
        let compact_time = t0.elapsed();
        done.store(true, std::sync::atomic::Ordering::Relaxed);
        let worst = writers.into_iter().map(|w| w.join().unwrap()).max().unwrap();
        assert!(worst < compact_time / 2, "a writer waited {worst:?}, the compaction took {compact_time:?}");
        let expected = e.snapshot();
        drop(e);
        let reopened = TestEngine::open(dir.path().join("e.log"), "test").unwrap();
        assert_eq!(reopened.snapshot(), expected, "the compacted log replays to the final index");
    }

    #[test]
    fn floor_with_a_key_before_the_prefix_finds_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let e = open(&dir);
        e.put("b/1".into(), 1).unwrap();
        assert_eq!(e.floor("b/", "a", |_| true), None);
        assert_eq!(e.floor("b/", "b/9", |_| true), Some(("b/1".to_string(), 1)));
    }

    #[test]
    fn floor_with_a_key_past_the_prefix_finds_the_last_prefixed_key() {
        let dir = tempfile::tempdir().unwrap();
        let e = open(&dir);
        for (k, v) in [("a/1", 1), ("a/2", 2), ("b", 3), ("c", 4)] {
            e.put(k.into(), v).unwrap();
        }
        assert_eq!(e.floor("a/", "c", |_| true), Some(("a/2".to_string(), 2)));
        assert_eq!(e.floor("a/", "c", |v| *v == 1), Some(("a/1".to_string(), 1)));
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
        let e = TestEngine::open(&path, "test").unwrap();
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
        assert_eq!(TestEngine::open(&path, "test").unwrap().get("k1"), Some(7));
        std::fs::write(&path, "garbage\n[\"k1\",7]\n").unwrap();
        assert!(TestEngine::open(&path, "test").is_err());
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
    fn delete_if_removes_only_when_keep_says_no_and_survives_replay() {
        let dir = tempfile::tempdir().unwrap();
        let e = open(&dir);
        e.put("k".into(), 5).unwrap();
        assert!(!e.delete_if("k", |v| *v != 4).unwrap(), "the value moved on: kept");
        assert!(!e.delete_if("missing", |_| false).unwrap());
        assert_eq!(e.get("k"), Some(5));
        assert!(e.delete_if("k", |v| *v != 5).unwrap());
        assert_eq!(e.get("k"), None);
        assert_eq!(open(&dir).get("k"), None, "the delete is in the log");
    }

    #[test]
    fn delete_many_if_removes_only_the_unchanged_and_survives_replay() {
        let dir = tempfile::tempdir().unwrap();
        let e = open(&dir);
        for i in 0..10_000u64 {
            e.put(format!("k{i}"), i).unwrap();
        }
        e.put("k5".into(), 999).unwrap(); // changed since the caller looked
        let keys: Vec<String> = (0..10_000).map(|i| format!("k{i}")).chain(["missing".to_string()]).collect();
        let removed = e.delete_many_if(&keys, |k, v| *v != k[1..].parse::<u64>().unwrap()).unwrap();
        assert_eq!(removed, 9_999);
        assert_eq!(e.get("k5"), Some(999));
        let replayed = open(&dir);
        assert_eq!(replayed.get("k5"), Some(999));
        assert_eq!(replayed.get("k6"), None);
        assert_eq!(replayed.snapshot().len(), 1);
    }

    #[test]
    fn floor_finds_the_nearest_key_at_or_below_within_the_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let e = open(&dir);
        for (k, v) in [("a/10", 1), ("a/20", 2), ("b/05", 3)] {
            e.put(k.into(), v).unwrap();
        }
        assert_eq!(e.floor("a/", "a/15", |_| true), Some(("a/10".into(), 1)));
        assert_eq!(e.floor("a/", "a/20", |_| true), Some(("a/20".into(), 2)));
        assert_eq!(e.floor("a/", "a/99", |_| true), Some(("a/20".into(), 2)));
        assert_eq!(e.floor("a/", "a/05", |_| true), None);
        assert_eq!(e.floor("b/", "b/04", |_| true), None, "a key in another prefix isn't returned");
        assert_eq!(e.floor("a/", "a/99", |v| *v == 1), Some(("a/10".into(), 1)), "entries accept refuses are skipped");
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
