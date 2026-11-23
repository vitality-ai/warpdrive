//! Any ordered key-value store as a WarpMetaStore engine.
//!
//! [`OrderedKv`] is the minimum a storage library has to provide: get a key,
//! write a batch atomically and durably, and walk keys in byte order from a
//! starting point. [`KvEngine`] builds the whole [`Engine`] on top of it once
//! (value encoding, conditional writes, prefix and range scans, floor
//! lookups, batched deletes), so a new store (RocksDB, LMDB, ...) is only an
//! adapter that maps these four calls onto its API.
//!
//! - Keys are their UTF-8 bytes. A store that compares keys bytewise keeps
//!   the order a `BTreeMap<String, _>` keeps, so scans and floor lookups
//!   answer as the Bitcask engine does.
//! - Values are JSON, the encoding the Bitcask log uses.
//! - Conditional writes (`put_if`, `delete_if`, `delete_many_if`) read, check
//!   and write under a lock stripe chosen by the key. Every write to a key
//!   takes that key's stripe, so a check always sees the latest value of its
//!   key, while writes to different keys run in parallel and the store can
//!   batch their syncs.

use parking_lot::{Mutex, MutexGuard};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::io;
use std::marker::PhantomData;

use crate::engine::Engine;

/// A durable key-value store that keeps keys in bytewise order.
pub trait OrderedKv: Send + Sync {
    fn get(&self, key: &[u8]) -> io::Result<Option<Vec<u8>>>;
    /// Applies every write atomically and returns once they're durable.
    /// `Some(value)` sets the key, `None` removes it (a missing key is fine).
    fn write(&self, batch: &[(&[u8], Option<&[u8]>)]) -> io::Result<()>;
    /// Visits entries in key order until `f` returns false: ascending from
    /// the first key at least `from`, or descending from the last key at
    /// most `from`.
    fn scan(&self, from: &[u8], descending: bool, f: &mut dyn FnMut(&[u8], &[u8]) -> bool) -> io::Result<()>;
    /// Reclaims space from overwritten and deleted entries. Stores that do
    /// this on their own leave it as is.
    fn compact(&self) -> io::Result<()> {
        Ok(())
    }
}

const STRIPES: usize = 1024;

/// [`Engine`] on any [`OrderedKv`].
pub struct KvEngine<V> {
    kv: Box<dyn OrderedKv>,
    stripes: Vec<Mutex<()>>,
    _v: PhantomData<fn() -> V>,
}

fn io_err(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}

fn encode<V: Serialize>(v: &V) -> io::Result<Vec<u8>> {
    serde_json::to_vec(v).map_err(io_err)
}

fn decode<V: DeserializeOwned>(b: &[u8]) -> Option<V> {
    serde_json::from_slice(b).ok()
}

fn stripe_of(key: &str) -> usize {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut h);
    h.finish() as usize % STRIPES
}

impl<V> KvEngine<V> {
    pub fn new(kv: Box<dyn OrderedKv>) -> Self {
        Self { kv, stripes: (0..STRIPES).map(|_| Mutex::new(())).collect(), _v: PhantomData }
    }

    fn lock(&self, key: &str) -> MutexGuard<'_, ()> {
        self.stripes[stripe_of(key)].lock()
    }
}

impl<V> KvEngine<V>
where
    V: Serialize + DeserializeOwned + Clone + Send + Sync,
{
    fn read(&self, key: &str) -> io::Result<Option<V>> {
        Ok(self.kv.get(key.as_bytes())?.and_then(|b| decode(&b)))
    }

    fn set(&self, key: &str, bytes: &[u8]) -> io::Result<()> {
        self.kv.write(&[(key.as_bytes(), Some(bytes))])
    }

    fn remove(&self, key: &str) -> io::Result<()> {
        self.kv.write(&[(key.as_bytes(), None)])
    }

    /// Decoded entries from `from` in key order until `f` says stop. Entries
    /// that aren't UTF-8 keys or don't decode are skipped.
    fn walk(&self, from: &str, descending: bool, mut f: impl FnMut(&str, V) -> bool) {
        let _ = self.kv.scan(from.as_bytes(), descending, &mut |k, v| {
            let (Ok(k), Some(v)) = (std::str::from_utf8(k), decode::<V>(v)) else { return true };
            f(k, v)
        });
    }
}

impl<V> Engine<V> for KvEngine<V>
where
    V: Serialize + DeserializeOwned + Clone + Send + Sync,
{
    fn get(&self, key: &str) -> Option<V> {
        self.read(key).ok().flatten()
    }

    fn put(&self, key: String, value: V) -> io::Result<()> {
        let bytes = encode(&value)?;
        let _g = self.lock(&key);
        self.set(&key, &bytes)
    }

    fn delete(&self, key: &str) -> io::Result<()> {
        let _g = self.lock(key);
        self.remove(key)
    }

    fn put_if<T>(&self, key: String, value: V, check: impl FnOnce(Option<&V>) -> Result<(), T>) -> io::Result<Result<Option<V>, T>> {
        let bytes = encode(&value)?;
        let _g = self.lock(&key);
        let current = self.read(&key)?;
        if let Err(e) = check(current.as_ref()) {
            return Ok(Err(e));
        }
        self.set(&key, &bytes)?;
        Ok(Ok(current))
    }

    fn delete_if(&self, key: &str, keep: impl FnOnce(&V) -> bool) -> io::Result<bool> {
        let _g = self.lock(key);
        match self.read(key)? {
            Some(v) if !keep(&v) => {}
            _ => return Ok(false),
        }
        self.remove(key)?;
        Ok(true)
    }

    fn delete_many_if(&self, keys: &[String], keep: impl Fn(&str, &V) -> bool) -> io::Result<usize> {
        // Every key's stripe, taken in stripe order so two batches can't
        // deadlock, then one durable batch for all the deletes.
        let mut stripes: Vec<usize> = keys.iter().map(|k| stripe_of(k)).collect();
        stripes.sort_unstable();
        stripes.dedup();
        let _guards: Vec<_> = stripes.iter().map(|&s| self.stripes[s].lock()).collect();
        // A key named twice is removed once: reads don't see the batch yet.
        let mut gone: HashSet<&str> = HashSet::new();
        for k in keys {
            if !gone.contains(k.as_str()) && matches!(self.read(k)?, Some(v) if !keep(k, &v)) {
                gone.insert(k.as_str());
            }
        }
        if !gone.is_empty() {
            let batch: Vec<(&[u8], Option<&[u8]>)> = gone.iter().map(|k| (k.as_bytes(), None)).collect();
            self.kv.write(&batch)?;
        }
        Ok(gone.len())
    }

    fn scan_range<T>(&self, after: Option<&str>, upto: &str, mut f: impl FnMut(&str, &V) -> Option<T>) -> Vec<T> {
        let mut out = Vec::new();
        self.walk(after.unwrap_or(""), false, |k, v| {
            if after.is_some_and(|a| k <= a) {
                return true;
            }
            if k > upto {
                return false;
            }
            if let Some(t) = f(k, &v) {
                out.push(t);
            }
            true
        });
        out
    }

    fn scan_prefix<T>(&self, prefix: &str, mut f: impl FnMut(&str, &V) -> Option<T>) -> Vec<T> {
        let mut out = Vec::new();
        self.walk(prefix, false, |k, v| {
            if !k.starts_with(prefix) {
                return false;
            }
            if let Some(t) = f(k, &v) {
                out.push(t);
            }
            true
        });
        out
    }

    fn floor(&self, prefix: &str, key: &str, accept: impl Fn(&V) -> bool) -> Option<(String, V)> {
        let mut found = None;
        self.walk(key, true, |k, v| {
            if !k.starts_with(prefix) {
                // Keys with the prefix are contiguous. Above them, keep going
                // down. Below them, stop.
                return k > prefix;
            }
            if accept(&v) {
                found = Some((k.to_string(), v));
                return false;
            }
            true
        });
        found
    }

    fn snapshot(&self) -> Vec<(String, V)> {
        let mut out = Vec::new();
        self.walk("", false, |k, v| {
            out.push((k.to_string(), v));
            true
        });
        out
    }

    fn compact(&self) -> io::Result<()> {
        self.kv.compact()
    }
}

/// An in-memory [`OrderedKv`], for tests of the generic layer and of code
/// that takes any engine.
#[derive(Default)]
pub struct MemKv(Mutex<std::collections::BTreeMap<Vec<u8>, Vec<u8>>>);

impl OrderedKv for MemKv {
    fn get(&self, key: &[u8]) -> io::Result<Option<Vec<u8>>> {
        Ok(self.0.lock().get(key).cloned())
    }
    fn write(&self, batch: &[(&[u8], Option<&[u8]>)]) -> io::Result<()> {
        let mut m = self.0.lock();
        for (k, v) in batch {
            match v {
                Some(v) => m.insert(k.to_vec(), v.to_vec()),
                None => m.remove(*k),
            };
        }
        Ok(())
    }
    fn scan(&self, from: &[u8], descending: bool, f: &mut dyn FnMut(&[u8], &[u8]) -> bool) -> io::Result<()> {
        let m = self.0.lock().clone();
        if descending {
            for (k, v) in m.range(..=from.to_vec()).rev() {
                if !f(k, v) {
                    break;
                }
            }
        } else {
            for (k, v) in m.range(from.to_vec()..) {
                if !f(k, v) {
                    break;
                }
            }
        }
        Ok(())
    }
}

/// Checks an engine against the Bitcask engine: the same random operations
/// on both must give the same answers after every step. Every `OrderedKv`
/// adapter runs it in its own tests.
pub fn assert_behaves_like_bitcask(e: &impl Engine<u64>, dir: &std::path::Path) {
    let bc = crate::BitcaskEngine::<u64>::open(dir.join("reference.log"), "reference").unwrap();
    // Keys share prefixes and include a multi-byte character and 0x7f, so
    // ordering and prefix bounds are exercised.
    let keys = ["a", "a/1", "a/2", "a/10", "ab", "b", "b/é", "b/\u{7f}", "c/x/y", "c/x", "c"];
    let prefixes = ["", "a", "a/", "b/", "c/x", "z"];
    let mut seed: u64 = 0x9e3779b97f4a7c15;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for step in 0..4000 {
        let k = keys[(next() % keys.len() as u64) as usize];
        let v = next() % 100;
        match next() % 8 {
            0 | 1 => {
                e.put(k.into(), v).unwrap();
                bc.put(k.into(), v).unwrap();
            }
            2 => {
                e.delete(k).unwrap();
                bc.delete(k).unwrap();
            }
            3 => {
                let limit = next() % 100;
                let a = e.put_if(k.into(), v, |c| if c.map_or(true, |c| *c < limit) { Ok(()) } else { Err(()) }).unwrap();
                let b = bc.put_if(k.into(), v, |c| if c.map_or(true, |c| *c < limit) { Ok(()) } else { Err(()) }).unwrap();
                assert_eq!(a, b, "put_if differs at step {step}");
            }
            4 => {
                let p = prefixes[(next() % prefixes.len() as u64) as usize];
                let a = e.scan_prefix(p, |k, v| Some((k.to_string(), *v)));
                let b = bc.scan_prefix(p, |k, v| Some((k.to_string(), *v)));
                assert_eq!(a, b, "scan_prefix({p:?}) differs at step {step}");
            }
            5 => {
                let (lo, hi) = (keys[(next() % keys.len() as u64) as usize], keys[(next() % keys.len() as u64) as usize]);
                let after = if next() % 3 == 0 { None } else { Some(lo) };
                let a = e.scan_range(after, hi, |k, v| Some((k.to_string(), *v)));
                let b = bc.scan_range(after, hi, |k, v| Some((k.to_string(), *v)));
                assert_eq!(a, b, "scan_range({after:?}, {hi:?}) differs at step {step}");
            }
            6 => {
                let ks: Vec<String> = (0..3).map(|_| keys[(next() % keys.len() as u64) as usize].to_string()).collect();
                let limit = next() % 100;
                let a = e.delete_many_if(&ks, |_, v| *v >= limit).unwrap();
                let b = bc.delete_many_if(&ks, |_, v| *v >= limit).unwrap();
                assert_eq!(a, b, "delete_many_if differs at step {step}");
            }
            _ => {
                let p = prefixes[(next() % prefixes.len() as u64) as usize];
                let even = next() % 2 == 0;
                let a = e.floor(p, k, |v| (*v % 2 == 0) == even);
                let b = bc.floor(p, k, |v| (*v % 2 == 0) == even);
                assert_eq!(a, b, "floor({p:?}, {k:?}) differs at step {step}");
            }
        }
        assert_eq!(e.get(k), bc.get(k), "get({k:?}) differs at step {step}");
    }
    assert_eq!(e.snapshot(), bc.snapshot());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_generic_engine_behaves_like_bitcask() {
        let dir = tempfile::tempdir().unwrap();
        assert_behaves_like_bitcask(&KvEngine::<u64>::new(Box::new(MemKv::default())), dir.path());
    }

    #[test]
    fn put_if_is_atomic_under_concurrent_writers() {
        let e = std::sync::Arc::new(KvEngine::<u64>::new(Box::new(MemKv::default())));
        e.put("n".into(), 0).unwrap();
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let e = e.clone();
                std::thread::spawn(move || {
                    for _ in 0..50 {
                        loop {
                            let cur = e.get("n").unwrap();
                            if e.put_if("n".into(), cur + 1, |c| if c == Some(&cur) { Ok(()) } else { Err(()) }).unwrap().is_ok() {
                                break;
                            }
                        }
                    }
                })
            })
            .collect();
        handles.into_iter().for_each(|h| h.join().unwrap());
        assert_eq!(e.get("n"), Some(400), "every increment applied exactly once");
    }
}
