//! SQLite engine for WarpMetaStore.
//!
//! [`SqliteEngine`] implements [`warp_meta_store::Engine`] on one SQLite
//! table, so [`SqliteMetaStore`] is a [`WarpMetaStore`] with the same
//! ordering, tombstones and replication rules as the Bitcask default. Only
//! where the bytes live changes.
//!
//! - Keys are stored as BLOBs. SQLite compares BLOBs byte by byte, which is
//!   the order a `BTreeMap<String, _>` keeps, so prefix scans and floor
//!   lookups return what the Bitcask engine returns.
//! - Values are JSON, the encoding the Bitcask log uses.
//! - Durability: WAL journal with `synchronous=FULL`. A write returns once
//!   its transaction is synced, as `Engine` requires.
//! - One connection behind a mutex. Every write is its own synced
//!   transaction, so concurrent writers each pay a sync: there is no group
//!   commit yet.

use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::io;
use std::marker::PhantomData;
use std::path::Path;
use warp_meta_store::{Engine, Entry, WarpMetaStore};

/// A [`WarpMetaStore`] whose records live in SQLite.
pub type SqliteMetaStore<R> = WarpMetaStore<R, SqliteEngine<Entry<R>>>;

/// [`Engine`] on a SQLite table `kv(key BLOB PRIMARY KEY, value BLOB)`.
pub struct SqliteEngine<V> {
    conn: Mutex<Connection>,
    _v: PhantomData<fn() -> V>,
}

fn io_err(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}

impl<V> SqliteEngine<V> {
    /// Opens (creating if needed) the database at `path`.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let conn = Connection::open(path).map_err(io_err)?;
        conn.pragma_update(None, "journal_mode", "WAL").map_err(io_err)?;
        conn.pragma_update(None, "synchronous", "FULL").map_err(io_err)?;
        conn.execute_batch("CREATE TABLE IF NOT EXISTS kv (key BLOB PRIMARY KEY, value BLOB NOT NULL) WITHOUT ROWID;")
            .map_err(io_err)?;
        Ok(Self { conn: Mutex::new(conn), _v: PhantomData })
    }
}

fn encode<V: Serialize>(v: &V) -> io::Result<Vec<u8>> {
    serde_json::to_vec(v).map_err(io_err)
}

fn decode<V: DeserializeOwned>(b: &[u8]) -> Option<V> {
    serde_json::from_slice(b).ok()
}

/// The smallest byte string greater than every string starting with
/// `prefix`, or `None` when there is none (empty or all 0xFF).
fn prefix_end(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut end = prefix.to_vec();
    while let Some(last) = end.pop() {
        if last < 0xFF {
            end.push(last + 1);
            return Some(end);
        }
    }
    None
}

impl<V> SqliteEngine<V>
where
    V: Serialize + DeserializeOwned + Clone + Send + Sync,
{
    /// Rows with keys starting with `prefix` and at most `upper` when given,
    /// in key order (descending when `desc`), passed to `f` until it says stop.
    fn walk(&self, prefix: &str, upper: Option<&str>, desc: bool, mut f: impl FnMut(String, V) -> bool) {
        let conn = self.conn.lock();
        let lo = prefix.as_bytes().to_vec();
        let hi = prefix_end(prefix.as_bytes());
        let order = if desc { "DESC" } else { "ASC" };
        let mut sql = String::from("SELECT key, value FROM kv WHERE key >= ?1");
        let mut args: Vec<Vec<u8>> = vec![lo];
        if let Some(hi) = hi {
            sql.push_str(&format!(" AND key < ?{}", args.len() + 1));
            args.push(hi);
        }
        if let Some(u) = upper {
            sql.push_str(&format!(" AND key <= ?{}", args.len() + 1));
            args.push(u.as_bytes().to_vec());
        }
        sql.push_str(&format!(" ORDER BY key {order}"));
        let Ok(mut stmt) = conn.prepare_cached(&sql) else { return };
        let Ok(mut rows) = stmt.query(rusqlite::params_from_iter(args.iter())) else { return };
        while let Ok(Some(row)) = rows.next() {
            let (Ok(k), Ok(v)) = (row.get::<_, Vec<u8>>(0), row.get::<_, Vec<u8>>(1)) else { continue };
            let (Ok(k), Some(v)) = (String::from_utf8(k), decode::<V>(&v)) else { continue };
            if !f(k, v) {
                break;
            }
        }
    }
}

impl<V> Engine<V> for SqliteEngine<V>
where
    V: Serialize + DeserializeOwned + Clone + Send + Sync,
{
    fn get(&self, key: &str) -> Option<V> {
        let conn = self.conn.lock();
        let b: Option<Vec<u8>> = conn
            .query_row("SELECT value FROM kv WHERE key = ?1", params![key.as_bytes()], |r| r.get(0))
            .optional()
            .ok()
            .flatten();
        b.and_then(|b| decode(&b))
    }

    fn put(&self, key: String, value: V) -> io::Result<()> {
        let bytes = encode(&value)?;
        let conn = self.conn.lock();
        conn.execute("INSERT OR REPLACE INTO kv (key, value) VALUES (?1, ?2)", params![key.as_bytes(), bytes]).map_err(io_err)?;
        Ok(())
    }

    fn delete(&self, key: &str) -> io::Result<()> {
        let conn = self.conn.lock();
        conn.execute("DELETE FROM kv WHERE key = ?1", params![key.as_bytes()]).map_err(io_err)?;
        Ok(())
    }

    fn put_if<T>(&self, key: String, value: V, check: impl FnOnce(Option<&V>) -> Result<(), T>) -> io::Result<Result<Option<V>, T>> {
        let bytes = encode(&value)?;
        let mut conn = self.conn.lock();
        // The connection lock already excludes other writers in this
        // process. The IMMEDIATE transaction keeps the read and the write
        // atomic against any other connection to the same file too.
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(io_err)?;
        let current: Option<V> = tx
            .query_row("SELECT value FROM kv WHERE key = ?1", params![key.as_bytes()], |r| r.get::<_, Vec<u8>>(0))
            .optional()
            .map_err(io_err)?
            .and_then(|b| decode(&b));
        if let Err(e) = check(current.as_ref()) {
            return Ok(Err(e));
        }
        tx.execute("INSERT OR REPLACE INTO kv (key, value) VALUES (?1, ?2)", params![key.as_bytes(), bytes]).map_err(io_err)?;
        tx.commit().map_err(io_err)?;
        Ok(Ok(current))
    }

    fn delete_if(&self, key: &str, keep: impl FnOnce(&V) -> bool) -> io::Result<bool> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(io_err)?;
        let current: Option<V> = tx
            .query_row("SELECT value FROM kv WHERE key = ?1", params![key.as_bytes()], |r| r.get::<_, Vec<u8>>(0))
            .optional()
            .map_err(io_err)?
            .and_then(|b| decode(&b));
        match current {
            Some(v) if !keep(&v) => {}
            _ => return Ok(false),
        }
        tx.execute("DELETE FROM kv WHERE key = ?1", params![key.as_bytes()]).map_err(io_err)?;
        tx.commit().map_err(io_err)?;
        Ok(true)
    }

    fn delete_many_if(&self, keys: &[String], keep: impl Fn(&str, &V) -> bool) -> io::Result<usize> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(io_err)?;
        let mut removed = 0;
        for key in keys {
            let current: Option<V> = tx
                .query_row("SELECT value FROM kv WHERE key = ?1", params![key.as_bytes()], |r| r.get::<_, Vec<u8>>(0))
                .optional()
                .map_err(io_err)?
                .and_then(|b| decode(&b));
            if matches!(current, Some(v) if !keep(key, &v)) {
                tx.execute("DELETE FROM kv WHERE key = ?1", params![key.as_bytes()]).map_err(io_err)?;
                removed += 1;
            }
        }
        tx.commit().map_err(io_err)?;
        Ok(removed)
    }

    fn scan_prefix<T>(&self, prefix: &str, mut f: impl FnMut(&str, &V) -> Option<T>) -> Vec<T> {
        let mut out = Vec::new();
        self.walk(prefix, None, false, |k, v| {
            if let Some(t) = f(&k, &v) {
                out.push(t);
            }
            true
        });
        out
    }

    fn floor(&self, prefix: &str, key: &str, accept: impl Fn(&V) -> bool) -> Option<(String, V)> {
        let mut found = None;
        self.walk(prefix, Some(key), true, |k, v| {
            if accept(&v) {
                found = Some((k, v));
                false
            } else {
                true
            }
        });
        found
    }

    fn snapshot(&self) -> Vec<(String, V)> {
        let mut out = Vec::new();
        self.walk("", None, false, |k, v| {
            out.push((k, v));
            true
        });
        out
    }

    fn compact(&self) -> io::Result<()> {
        let conn = self.conn.lock();
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM;").map_err(io_err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use warp_meta_store::{Applied, BitcaskEngine, Record};

    #[test]
    fn prefix_end_is_the_next_byte_string() {
        assert_eq!(prefix_end(b"ab"), Some(b"ac".to_vec()));
        assert_eq!(prefix_end(b"a\xff"), Some(b"b".to_vec()));
        assert_eq!(prefix_end(b"\xff\xff"), None);
        assert_eq!(prefix_end(b""), None);
    }

    #[test]
    fn put_get_delete_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kv.db");
        {
            let e = SqliteEngine::<u64>::open(&path).unwrap();
            e.put("a".into(), 1).unwrap();
            e.put("b".into(), 2).unwrap();
            e.put("a".into(), 3).unwrap();
            e.delete("b").unwrap();
            e.delete("missing").unwrap();
        }
        let e = SqliteEngine::<u64>::open(&path).unwrap();
        assert_eq!(e.get("a"), Some(3));
        assert_eq!(e.get("b"), None);
        e.compact().unwrap();
        assert_eq!(e.snapshot(), vec![("a".to_string(), 3)]);
    }

    #[test]
    fn put_if_writes_only_when_the_check_passes() {
        let dir = tempfile::tempdir().unwrap();
        let e = SqliteEngine::<u64>::open(dir.path().join("kv.db")).unwrap();
        assert_eq!(e.put_if("k".into(), 5, |cur| if cur.is_none() { Ok(()) } else { Err("exists") }).unwrap(), Ok(None));
        assert_eq!(e.put_if("k".into(), 6, |cur| if cur.is_none() { Ok(()) } else { Err("exists") }).unwrap(), Err("exists"));
        assert_eq!(e.get("k"), Some(5));
        assert_eq!(e.put_if("k".into(), 7, |cur| if cur == Some(&5) { Ok(()) } else { Err("changed") }).unwrap(), Ok(Some(5)));
        assert_eq!(e.get("k"), Some(7));
    }

    #[test]
    fn delete_if_removes_only_when_keep_says_no() {
        let dir = tempfile::tempdir().unwrap();
        let e = SqliteEngine::<u64>::open(dir.path().join("kv.db")).unwrap();
        e.put("k".into(), 5).unwrap();
        assert!(!e.delete_if("k", |v| *v != 4).unwrap());
        assert!(!e.delete_if("missing", |_| false).unwrap());
        assert_eq!(e.get("k"), Some(5));
        assert!(e.delete_if("k", |v| *v != 5).unwrap());
        assert_eq!(e.get("k"), None);
    }

    #[test]
    fn put_if_is_atomic_under_concurrent_writers() {
        let dir = tempfile::tempdir().unwrap();
        let e = std::sync::Arc::new(SqliteEngine::<u64>::open(dir.path().join("kv.db")).unwrap());
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

    /// Runs the same random operations on SQLite and Bitcask and requires
    /// the same answer from both after every step.
    #[test]
    fn behaves_like_the_bitcask_engine() {
        let dir = tempfile::tempdir().unwrap();
        let sq = SqliteEngine::<u64>::open(dir.path().join("kv.db")).unwrap();
        let bc = BitcaskEngine::<u64>::open(dir.path().join("kv.log"), "diff").unwrap();
        // Keys share prefixes and include a multi-byte character and 0x7f,
        // so ordering and prefix bounds are exercised.
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
            match next() % 6 {
                0 | 1 => {
                    sq.put(k.into(), v).unwrap();
                    bc.put(k.into(), v).unwrap();
                }
                2 => {
                    sq.delete(k).unwrap();
                    bc.delete(k).unwrap();
                }
                3 => {
                    let limit = next() % 100;
                    let a = sq.put_if(k.into(), v, |c| if c.map_or(true, |c| *c < limit) { Ok(()) } else { Err(()) }).unwrap();
                    let b = bc.put_if(k.into(), v, |c| if c.map_or(true, |c| *c < limit) { Ok(()) } else { Err(()) }).unwrap();
                    assert_eq!(a, b, "put_if differs at step {step}");
                }
                4 => {
                    let p = prefixes[(next() % prefixes.len() as u64) as usize];
                    let a = sq.scan_prefix(p, |k, v| Some((k.to_string(), *v)));
                    let b = bc.scan_prefix(p, |k, v| Some((k.to_string(), *v)));
                    assert_eq!(a, b, "scan_prefix({p:?}) differs at step {step}");
                }
                _ => {
                    let p = prefixes[(next() % prefixes.len() as u64) as usize];
                    let even = next() % 2 == 0;
                    let a = sq.floor(p, k, |v| (*v % 2 == 0) == even);
                    let b = bc.floor(p, k, |v| (*v % 2 == 0) == even);
                    assert_eq!(a, b, "floor({p:?}, {k:?}) differs at step {step}");
                }
            }
            assert_eq!(sq.get(k), bc.get(k), "get({k:?}) differs at step {step}");
        }
        assert_eq!(sq.snapshot(), bc.snapshot());
    }

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    struct Rec {
        seq: u64,
        version: String,
        v: String,
    }
    impl Record for Rec {
        fn seq(&self) -> u64 {
            self.seq
        }
        fn version(&self) -> &str {
            &self.version
        }
    }
    fn rec(seq: u64, v: &str) -> Rec {
        Rec { seq, version: format!("v{seq}"), v: v.to_string() }
    }

    #[test]
    fn sqlite_meta_store_keeps_ordering_and_tombstones_across_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("meta.db");
        {
            let s: SqliteMetaStore<Rec> = WarpMetaStore::new(SqliteEngine::open(&path).unwrap());
            assert!(matches!(s.put("k".into(), rec(10, "new")).unwrap(), Applied::Yes { .. }));
            assert!(matches!(s.put("k".into(), rec(5, "old")).unwrap(), Applied::Stale { current_seq: 10 }), "an older write is refused");
            assert!(matches!(s.delete("k".into(), 20).unwrap(), Applied::Yes { .. }));
        }
        let s: SqliteMetaStore<Rec> = WarpMetaStore::new(SqliteEngine::open(&path).unwrap());
        assert_eq!(s.get("k"), None, "the delete survives the restart");
        assert!(matches!(s.put("k".into(), rec(15, "late")).unwrap(), Applied::Stale { current_seq: 20 }), "the tombstone refuses an older late write");
        assert!(matches!(s.put("k".into(), rec(25, "newer")).unwrap(), Applied::Yes { .. }));
        assert_eq!(s.get("k").map(|r| r.v), Some("newer".to_string()));
    }
}
