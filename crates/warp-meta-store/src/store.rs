//! Ordered record writes over any `Engine`.
//!
//! Every location record and tombstone carries a `seq` (see `hlc`). A node
//! applies a write only if it is newer than what it holds for the key, so
//! every node ends on the same record whatever order writes arrive in.
//! Deletes leave a tombstone with its `seq`, so a late, older write can't
//! bring a deleted object back.
//!
//! The rules live here once. An engine only has to provide `put_if`, an
//! atomic check-and-write, so every engine gets the same ordering.
//!
//! **Assumption:** writes for the cluster come in through a single node.
//! Under that, the coordinator's clock orders every write and replicas
//! converge on its last one. Writes through several nodes also converge,
//! newest stamp wins, but there's no per-key serialization: conditional
//! writes and read-modify-write updates can still race. Multiple writers
//! are later work.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};

use crate::hlc;
use crate::engine::Engine;
use std::io;

/// What a record must expose for ordered application.
pub trait Record: Clone + Serialize + DeserializeOwned + Send + Sync {
    fn seq(&self) -> u64;
    /// The object version this record describes. Its shards are stored
    /// under this version, so a write with a different version replaces
    /// those shards.
    fn version(&self) -> &str;
}

/// A key's state on this node.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum Entry<R> {
    Live(R),
    Tombstone { seq: u64 },
}

/// Reads both the tagged form this module writes and a bare record, which
/// is how a record line looked before ordering existed. A bare record
/// reads as live with its own `seq` (0 if it has none), so any later write
/// replaces it.
///
/// Parsed through `serde_json::Value`, not a serde untagged enum: an
/// untagged enum can't read a map with numeric keys (a multipart upload's
/// parts), so a node holding one couldn't replay its own log.
///
/// A binary format (the RocksDB engine's MessagePack) only ever holds the
/// tagged form, so it is read directly, without the JSON detour.
impl<'de, R: DeserializeOwned> Deserialize<'de> for Entry<R> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        if !d.is_human_readable() {
            #[derive(Deserialize)]
            #[serde(bound = "R: DeserializeOwned")]
            enum Tagged<R> {
                Live(R),
                Tombstone { seq: u64 },
            }
            return Ok(match Tagged::<R>::deserialize(d)? {
                Tagged::Live(r) => Entry::Live(r),
                Tagged::Tombstone { seq } => Entry::Tombstone { seq },
            });
        }
        let v = serde_json::Value::deserialize(d)?;
        if let serde_json::Value::Object(m) = &v {
            if m.len() == 1 {
                if let Some(r) = m.get("Live") {
                    return R::deserialize(r).map(Entry::Live).map_err(D::Error::custom);
                }
                if let Some(t) = m.get("Tombstone") {
                    let seq = t.get("seq").and_then(|s| s.as_u64()).ok_or_else(|| D::Error::custom("tombstone without a seq"))?;
                    return Ok(Entry::Tombstone { seq });
                }
            }
        }
        R::deserialize(&v).map(Entry::Live).map_err(D::Error::custom)
    }
}

impl<R: Record> Entry<R> {
    pub fn seq(&self) -> u64 {
        match self {
            Entry::Live(r) => r.seq(),
            Entry::Tombstone { seq } => *seq,
        }
    }

    pub fn live(&self) -> Option<&R> {
        match self {
            Entry::Live(r) => Some(r),
            Entry::Tombstone { .. } => None,
        }
    }

    pub fn into_live(self) -> Option<R> {
        match self {
            Entry::Live(r) => Some(r),
            Entry::Tombstone { .. } => None,
        }
    }
}

/// Outcome of a write.
#[derive(Debug, Clone, PartialEq)]
pub enum Applied<R> {
    /// The write is now the key's state. `previous` is what it replaced,
    /// `None` if the key was absent or the write was a repeat.
    Yes { previous: Option<Entry<R>> },
    /// The key already holds something at least as new. The sender should
    /// stamp above `current_seq` and send again.
    Stale { current_seq: u64 },
    /// A metadata update named a version that isn't the key's live version
    /// (it was overwritten or deleted). Not retried.
    VersionMismatch,
}

impl<R> Applied<R> {
    pub fn is_applied(&self) -> bool {
        matches!(self, Applied::Yes { .. })
    }
}

/// Why `put_if` refused a write.
enum Refused<R> {
    /// Not a failure: the same write arrived twice and is already applied.
    Repeat,
    Outcome(Applied<R>),
}

/// Ordered records on engine `E`.
pub struct WarpMetaStore<R, E> {
    engine: E,
    _record: std::marker::PhantomData<fn() -> R>,
}

impl<R: Record, E: Engine<Entry<R>>> WarpMetaStore<R, E> {
    pub fn new(engine: E) -> Self {
        Self { engine, _record: std::marker::PhantomData }
    }

    pub fn engine(&self) -> &E {
        &self.engine
    }

    fn write(&self, rk: String, entry: Entry<R>, decide: impl FnOnce(Option<&Entry<R>>) -> Result<(), Refused<R>>) -> io::Result<Applied<R>> {
        hlc::observe(entry.seq());
        let result = self.engine.put_if(rk, entry, |current| {
            if let Some(c) = current {
                hlc::observe(c.seq());
            }
            decide(current)
        })?;
        Ok(match result {
            Ok(previous) => Applied::Yes { previous },
            Err(Refused::Repeat) => Applied::Yes { previous: None },
            Err(Refused::Outcome(o)) => o,
        })
    }

    /// A new version of the object, or the same write sent again.
    pub fn put(&self, rk: String, record: R) -> io::Result<Applied<R>> {
        let seq = record.seq();
        let version = record.version().to_string();
        self.write(rk, Entry::Live(record), |current| match current {
            None => Ok(()),
            Some(c) if seq > c.seq() => Ok(()),
            Some(Entry::Live(c)) if seq == c.seq() && c.version() == version => Err(Refused::Repeat),
            Some(c) => Err(Refused::Outcome(Applied::Stale { current_seq: c.seq() })),
        })
    }

    /// A metadata change (tags, retention) to the version the record names.
    /// Applied while that version is the live one, or when this node has
    /// nothing for the key (it missed the PUT). Refused after an overwrite
    /// or a delete, so an update built from an old copy of the record can
    /// never bring back a version that's gone.
    pub fn update(&self, rk: String, record: R) -> io::Result<Applied<R>> {
        let seq = record.seq();
        let version = record.version().to_string();
        self.write(rk, Entry::Live(record), |current| match current {
            None => Ok(()),
            Some(Entry::Live(c)) if c.version() == version => {
                if seq > c.seq() {
                    Ok(())
                } else if seq == c.seq() {
                    Err(Refused::Repeat)
                } else {
                    Err(Refused::Outcome(Applied::Stale { current_seq: c.seq() }))
                }
            }
            _ => Err(Refused::Outcome(Applied::VersionMismatch)),
        })
    }

    /// Deletes the key at `seq`, leaving a tombstone.
    pub fn delete(&self, rk: String, seq: u64) -> io::Result<Applied<R>> {
        self.write(rk, Entry::Tombstone { seq }, |current| match current {
            None => Ok(()),
            Some(c) if seq > c.seq() => Ok(()),
            Some(Entry::Tombstone { seq: t }) if *t == seq => Err(Refused::Repeat),
            Some(c) => Err(Refused::Outcome(Applied::Stale { current_seq: c.seq() })),
        })
    }

    pub fn get(&self, rk: &str) -> Option<R> {
        self.engine.get(rk).and_then(Entry::into_live)
    }

    pub fn entry(&self, rk: &str) -> Option<Entry<R>> {
        self.engine.get(rk)
    }

    /// The live record with the greatest key at most `key` within `prefix`.
    pub fn floor_live(&self, prefix: &str, key: &str) -> Option<(String, R)> {
        // Tombstones are skipped: a deleted entry's predecessor is the answer.
        self.engine.floor(prefix, key, |e| e.live().is_some()).and_then(|(k, e)| e.into_live().map(|r| (k, r)))
    }

    /// `f` over every live record with `after < key <= upto`.
    pub fn list_live_range<T>(&self, after: Option<&str>, upto: &str, mut f: impl FnMut(&str, &R) -> T) -> Vec<T> {
        self.engine.scan_range(after, upto, |k, e| e.live().map(|r| f(k, r)))
    }

    /// `f` over every live record whose key starts with `prefix`.
    pub fn list_live<T>(&self, prefix: &str, mut f: impl FnMut(&str, &R) -> T) -> Vec<T> {
        self.engine.scan_prefix(prefix, |k, e| e.live().map(|r| f(k, r)))
    }

    /// Compacts the engine. Tombstones are kept: they're what stops an
    /// older write from being applied after the delete.
    pub fn compact(&self) -> io::Result<()> {
        self.engine.compact()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::BitcaskEngine;

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct Rec {
        #[serde(default)]
        seq: u64,
        version: String,
        #[serde(default)]
        tag: String,
    }

    impl Record for Rec {
        fn seq(&self) -> u64 {
            self.seq
        }
        fn version(&self) -> &str {
            &self.version
        }
    }

    fn rec(seq: u64, version: &str) -> Rec {
        Rec { seq, version: version.into(), tag: String::new() }
    }

    type TestStore = WarpMetaStore<Rec, BitcaskEngine<Entry<Rec>>>;

    fn open_at(path: &std::path::Path) -> TestStore {
        WarpMetaStore::new(BitcaskEngine::open(path, "test").unwrap())
    }

    fn open() -> (tempfile::TempDir, TestStore) {
        let dir = tempfile::tempdir().unwrap();
        let s = open_at(&dir.path().join("r.log"));
        (dir, s)
    }

    #[test]
    fn two_overwrites_in_either_arrival_order_end_on_the_same_record() {
        let (_d1, a) = open();
        let (_d2, b) = open();
        a.put("k".into(), rec(10, "v1")).unwrap();
        a.put("k".into(), rec(20, "v2")).unwrap();
        b.put("k".into(), rec(20, "v2")).unwrap();
        assert_eq!(b.put("k".into(), rec(10, "v1")).unwrap(), Applied::Stale { current_seq: 20 });
        assert_eq!(a.get("k"), b.get("k"));
        assert_eq!(a.get("k").unwrap().version, "v2");
    }

    #[test]
    fn an_older_write_after_a_delete_does_not_bring_the_object_back() {
        let (_d, s) = open();
        s.put("k".into(), rec(10, "v1")).unwrap();
        s.delete("k".into(), 30).unwrap();
        assert_eq!(s.put("k".into(), rec(20, "v2")).unwrap(), Applied::Stale { current_seq: 30 });
        assert!(s.get("k").is_none());
        assert_eq!(s.entry("k"), Some(Entry::Tombstone { seq: 30 }));
    }

    #[test]
    fn a_newer_write_after_a_delete_creates_the_object_again() {
        let (_d, s) = open();
        s.delete("k".into(), 30).unwrap();
        assert!(s.put("k".into(), rec(40, "v3")).unwrap().is_applied());
        assert_eq!(s.get("k").unwrap().version, "v3");
    }

    #[test]
    fn applying_reports_what_was_replaced() {
        let (_d, s) = open();
        s.put("k".into(), rec(10, "v1")).unwrap();
        assert_eq!(s.delete("k".into(), 20).unwrap(), Applied::Yes { previous: Some(Entry::Live(rec(10, "v1"))) });
    }

    #[test]
    fn the_same_write_sent_twice_is_accepted_both_times() {
        let (_d, s) = open();
        assert!(s.put("k".into(), rec(10, "v1")).unwrap().is_applied());
        assert!(s.put("k".into(), rec(10, "v1")).unwrap().is_applied());
        assert!(s.delete("k".into(), 20).unwrap().is_applied());
        assert!(s.delete("k".into(), 20).unwrap().is_applied());
    }

    #[test]
    fn an_update_for_an_overwritten_or_deleted_version_is_refused() {
        let (_d, s) = open();
        s.put("k".into(), rec(10, "v1")).unwrap();
        s.put("k".into(), rec(20, "v2")).unwrap();
        assert_eq!(s.update("k".into(), Rec { tag: "x".into(), ..rec(30, "v1") }).unwrap(), Applied::VersionMismatch);
        s.delete("k".into(), 40).unwrap();
        assert_eq!(s.update("k".into(), rec(50, "v2")).unwrap(), Applied::VersionMismatch);
    }

    #[test]
    fn an_update_for_the_live_version_or_a_missed_put_is_applied() {
        let (_d, s) = open();
        assert!(s.update("missed".into(), rec(5, "v1")).unwrap().is_applied());
        s.put("k".into(), rec(10, "v1")).unwrap();
        assert!(s.update("k".into(), Rec { tag: "x".into(), ..rec(30, "v1") }).unwrap().is_applied());
        assert_eq!(s.get("k").unwrap().tag, "x");
    }

    #[test]
    fn tombstones_and_order_survive_a_restart_and_a_compaction() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("r.log");
        {
            let s = open_at(&path);
            s.put("a".into(), rec(10, "v1")).unwrap();
            s.delete("a".into(), 20).unwrap();
            s.put("b".into(), rec(15, "v1")).unwrap();
            s.compact().unwrap();
        }
        let s = open_at(&path);
        assert_eq!(s.entry("a"), Some(Entry::Tombstone { seq: 20 }));
        assert_eq!(s.put("a".into(), rec(12, "v0")).unwrap(), Applied::Stale { current_seq: 20 });
        assert_eq!(s.get("b").unwrap(), rec(15, "v1"));
    }

    #[test]
    fn a_log_written_before_ordering_still_replays() {
        // A record line was `[key, record]` and a delete `[key, null]`.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("r.log");
        std::fs::write(&path, "[\"a\",{\"version\":\"v1\"}]\n[\"b\",{\"version\":\"v1\"}]\n[\"b\",null]\n").unwrap();
        let s = open_at(&path);
        assert_eq!(s.get("a"), Some(rec(0, "v1")));
        assert_eq!(s.entry("b"), None);
        assert!(s.put("a".into(), rec(1, "v2")).unwrap().is_applied(), "any stamped write replaces an old record");
    }

    #[test]
    fn listing_skips_tombstones() {
        let (_d, s) = open();
        s.put("p/a".into(), rec(10, "v1")).unwrap();
        s.put("p/b".into(), rec(10, "v1")).unwrap();
        s.delete("p/a".into(), 20).unwrap();
        assert_eq!(s.list_live("p/", |k, _| k.to_string()), vec!["p/b".to_string()]);
    }

    /// #155: the live state and a fresh replay must agree, however
    /// concurrent writers to one key interleave.
    #[test]
    fn concurrent_writers_never_leave_the_log_and_index_disagreeing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("r.log");
        let s = std::sync::Arc::new(open_at(&path));
        let handles: Vec<_> = (0..16)
            .map(|t| {
                let s = std::sync::Arc::clone(&s);
                std::thread::spawn(move || {
                    for i in 0..50u64 {
                        let seq = hlc::now();
                        if (t + i) % 2 == 0 {
                            let _ = s.put("k".into(), rec(seq, &format!("v{seq}")));
                        } else {
                            let _ = s.delete("k".into(), seq);
                        }
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(s.entry("k"), open_at(&path).entry("k"));
    }
}
