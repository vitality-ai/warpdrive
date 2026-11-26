//! The engine under the S3 metadata rows, chosen at startup.
//!
//! The metadata store is a `WarpMetaStore`. Its engine is RocksDB (the
//! default, through `KvEngine`), the KV engine (`BitcaskEngine`, from
//! `warp-meta-store`, every row in RAM), the SQL
//! engine (`SqliteEngine`, from `warp-meta-store-sqlite`) or another ordered
//! key-value store run through `warp_meta_store::KvEngine` (RocksDB comes
//! from `warp-meta-store-rocksdb`), picked with
//! `WARPDRIVE_META_ENGINE=bitcask|sqlite|rocksdb`. All give the same ordering
//! and answers. Where the bytes live, and so how memory grows, changes:
//! Bitcask keeps every row in RAM, RocksDB a bounded cache.
//!
//! A new key-value store needs an `OrderedKv` adapter and one line in
//! `open_kv`. Nothing else here or in the stores changes.
//!
//! `Engine` has generic methods, so it can't be a trait object. `AnyEngine`
//! is the enum that holds whichever engine was picked and forwards to it.

use serde::de::DeserializeOwned;
use serde::Serialize;
use std::io;
use std::path::Path;
use warp_meta_store::{BitcaskEngine, Engine, KvEngine, OrderedKv};
use warp_meta_store_rocksdb::RocksKv;
use warp_meta_store_sqlite::SqliteEngine;

/// Which engine stores the metadata rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetaEngineKind {
    /// Append-only log plus every row in memory.
    Bitcask,
    /// One SQLite table.
    Sqlite,
    /// RocksDB through `KvEngine`: rows on disk, memory bounded by a
    /// shared block cache. The default.
    Rocksdb,
}

impl MetaEngineKind {
    /// `WARPDRIVE_META_ENGINE`, `rocksdb` when unset.
    pub fn from_env() -> Result<Self, String> {
        match std::env::var("WARPDRIVE_META_ENGINE") {
            Err(_) => Ok(Self::Rocksdb),
            Ok(v) => Self::parse(&v),
        }
    }

    pub fn parse(v: &str) -> Result<Self, String> {
        match v.trim().to_ascii_lowercase().as_str() {
            "bitcask" | "kv" => Ok(Self::Bitcask),
            "sqlite" | "sql" => Ok(Self::Sqlite),
            "" | "rocksdb" | "rocks" => Ok(Self::Rocksdb),
            other => Err(format!("WARPDRIVE_META_ENGINE={other:?}: expected bitcask, sqlite or rocksdb")),
        }
    }

    /// The file the rows live in when `WARPDRIVE_META_LOG` doesn't name one.
    pub fn default_path(self) -> String {
        self.file_name("cluster_metadata")
    }

    /// Every engine.
    pub const ALL: [Self; 3] = [Self::Bitcask, Self::Sqlite, Self::Rocksdb];

    /// Refuses to start on a fresh store when another engine's store is
    /// sitting at its default name in the same directory: the node would
    /// otherwise come up empty next to its old metadata.
    pub fn refuse_other_engines_data(self, stem: &str, path: &str) -> Result<(), String> {
        if Path::new(path).exists() {
            return Ok(());
        }
        for other in Self::ALL.into_iter().filter(|o| *o != self) {
            let theirs = other.file_name(stem);
            if Path::new(&theirs).exists() {
                return Err(format!(
                    "{theirs} holds metadata written by the {other:?} engine, and this node is set to {self:?} (WARPDRIVE_META_ENGINE). \
                     Set WARPDRIVE_META_ENGINE={} to keep using it, or move it away to start empty. There is no migration between engines.",
                    format!("{other:?}").to_ascii_lowercase()
                ));
            }
        }
        Ok(())
    }

    /// `stem` with this engine's file extension.
    pub fn file_name(self, stem: &str) -> String {
        let ext = match self {
            Self::Bitcask => "log",
            Self::Sqlite => "db",
            Self::Rocksdb => "rocks",
        };
        format!("{stem}.{ext}")
    }
}

/// The key-value store behind `kind`, for kinds that run through `KvEngine`.
/// The one place a new store is named.
fn open_kv(kind: MetaEngineKind, path: &Path) -> Option<io::Result<Box<dyn OrderedKv>>> {
    match kind {
        MetaEngineKind::Rocksdb => Some(RocksKv::open(path).map(|kv| Box::new(kv) as Box<dyn OrderedKv>)),
        MetaEngineKind::Bitcask | MetaEngineKind::Sqlite => None,
    }
}

/// The engine picked at startup.
pub enum AnyEngine<V> {
    Bitcask(BitcaskEngine<V>),
    Sqlite(SqliteEngine<V>),
    /// Any ordered key-value store, through `KvEngine`.
    Kv(MetaEngineKind, KvEngine<V>),
}

/// The first bytes of every SQLite database file.
const SQLITE_HEADER: &[u8] = b"SQLite format 3\0";

impl<V> AnyEngine<V>
where
    V: Serialize + DeserializeOwned + Clone + Send + Sync,
{
    /// Opens `kind` at `path`. Refuses a file written by the other engine:
    /// SQLite rejects a file that isn't a database, and the Bitcask engine
    /// refuses a file that starts with the SQLite header instead of
    /// replaying it as log lines.
    pub fn open(kind: MetaEngineKind, path: impl AsRef<Path>, label: &'static str) -> io::Result<Self> {
        let path = path.as_ref();
        match kind {
            MetaEngineKind::Bitcask => {
                if let Ok(mut f) = std::fs::File::open(path) {
                    let mut head = [0u8; 16];
                    if std::io::Read::read_exact(&mut f, &mut head).is_ok() && head == SQLITE_HEADER {
                        return Err(io::Error::other(format!(
                            "{} is a SQLite database: start with WARPDRIVE_META_ENGINE=sqlite, or point WARPDRIVE_META_LOG at a Bitcask log",
                            path.display()
                        )));
                    }
                }
                Ok(Self::Bitcask(BitcaskEngine::open(path, label)?))
            }
            MetaEngineKind::Sqlite => Ok(Self::Sqlite(SqliteEngine::open(path).map_err(|e| {
                io::Error::other(format!("{}: {e} (a Bitcask log needs WARPDRIVE_META_ENGINE=bitcask)", path.display()))
            })?)),
            kv => {
                // Key-value stores keep a directory. A file here was written
                // by the Bitcask or SQLite engine.
                if path.is_file() {
                    return Err(io::Error::other(format!(
                        "{} is a file, not a {kv:?} directory: start with the engine that wrote it, or point WARPDRIVE_META_LOG elsewhere",
                        path.display()
                    )));
                }
                let store = open_kv(kv, path).expect("every other kind is a key-value store")?;
                Ok(Self::Kv(kv, KvEngine::new(store)))
            }
        }
    }

    pub fn kind(&self) -> MetaEngineKind {
        match self {
            Self::Bitcask(_) => MetaEngineKind::Bitcask,
            Self::Sqlite(_) => MetaEngineKind::Sqlite,
            Self::Kv(kind, _) => *kind,
        }
    }
}

macro_rules! forward {
    ($self:ident, $e:ident => $call:expr) => {
        match $self {
            AnyEngine::Bitcask($e) => $call,
            AnyEngine::Sqlite($e) => $call,
            AnyEngine::Kv(_, $e) => $call,
        }
    };
}

impl<V> Engine<V> for AnyEngine<V>
where
    V: Serialize + DeserializeOwned + Clone + Send + Sync,
{
    fn get(&self, key: &str) -> Option<V> {
        forward!(self, e => e.get(key))
    }
    fn put(&self, key: String, value: V) -> io::Result<()> {
        forward!(self, e => e.put(key, value))
    }
    fn delete(&self, key: &str) -> io::Result<()> {
        forward!(self, e => e.delete(key))
    }
    fn put_if<T>(&self, key: String, value: V, check: impl FnOnce(Option<&V>) -> Result<(), T>) -> io::Result<Result<Option<V>, T>> {
        forward!(self, e => e.put_if(key, value, check))
    }
    fn delete_if(&self, key: &str, keep: impl FnOnce(&V) -> bool) -> io::Result<bool> {
        forward!(self, e => e.delete_if(key, keep))
    }
    fn delete_many_if(&self, keys: &[String], keep: impl Fn(&str, &V) -> bool) -> io::Result<usize> {
        forward!(self, e => e.delete_many_if(keys, keep))
    }
    fn put_many_if<T>(&self, items: Vec<(String, V)>, check: impl Fn(usize, Option<&V>) -> Result<(), T>) -> io::Result<Vec<Result<Option<V>, T>>> {
        forward!(self, e => e.put_many_if(items, check))
    }
    fn scan_range<T>(&self, after: Option<&str>, upto: &str, f: impl FnMut(&str, &V) -> Option<T>) -> Vec<T> {
        forward!(self, e => e.scan_range(after, upto, f))
    }
    fn scan_prefix<T>(&self, prefix: &str, f: impl FnMut(&str, &V) -> Option<T>) -> Vec<T> {
        forward!(self, e => e.scan_prefix(prefix, f))
    }
    fn floor(&self, prefix: &str, key: &str, accept: impl Fn(&V) -> bool) -> Option<(String, V)> {
        forward!(self, e => e.floor(prefix, key, accept))
    }
    fn snapshot(&self) -> Vec<(String, V)> {
        forward!(self, e => e.snapshot())
    }
    fn compact(&self) -> io::Result<()> {
        forward!(self, e => e.compact())
    }
    fn compaction_due(&self) -> bool {
        forward!(self, e => e.compaction_due())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_setting() {
        assert_eq!(MetaEngineKind::parse("bitcask"), Ok(MetaEngineKind::Bitcask));
        assert_eq!(MetaEngineKind::parse(" SQLite "), Ok(MetaEngineKind::Sqlite));
        assert_eq!(MetaEngineKind::parse("RocksDB"), Ok(MetaEngineKind::Rocksdb));
        assert_eq!(MetaEngineKind::parse(""), Ok(MetaEngineKind::Rocksdb), "RocksDB is the default");
        assert!(MetaEngineKind::parse("leveldb").is_err());
    }

    #[test]
    fn a_node_refuses_to_start_empty_next_to_another_engines_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let (stem, at) = (dir.path().join("m").to_string_lossy().to_string(), |k: MetaEngineKind, s: &str| k.file_name(s));
        AnyEngine::<u64>::open(MetaEngineKind::Bitcask, at(MetaEngineKind::Bitcask, &stem), "test").unwrap().put("k".into(), 1).unwrap();
        let e = MetaEngineKind::Rocksdb.refuse_other_engines_data(&stem, &at(MetaEngineKind::Rocksdb, &stem)).unwrap_err();
        assert!(e.contains("Bitcask") && e.contains("WARPDRIVE_META_ENGINE=bitcask"), "{e}");
        assert!(MetaEngineKind::Bitcask.refuse_other_engines_data(&stem, &at(MetaEngineKind::Bitcask, &stem)).is_ok(), "its own data is fine");
    }

    #[test]
    fn every_engine_answers_the_same() {
        let dir = tempfile::tempdir().unwrap();
        let engines: Vec<AnyEngine<u64>> = vec![
            AnyEngine::open(MetaEngineKind::Bitcask, dir.path().join("m.log"), "test").unwrap(),
            AnyEngine::open(MetaEngineKind::Sqlite, dir.path().join("m.db"), "test").unwrap(),
            AnyEngine::open(MetaEngineKind::Rocksdb, dir.path().join("m.rocks"), "test").unwrap(),
        ];
        for e in &engines {
            e.put("a/1".into(), 1).unwrap();
            e.put("a/2".into(), 2).unwrap();
            e.put("b".into(), 3).unwrap();
            e.delete("b").unwrap();
        }
        let answers: Vec<_> = engines.iter().map(|e| (e.snapshot(), e.floor("a/", "a/9", |_| true), e.scan_prefix("a/", |k, v| Some((k.to_string(), *v))))).collect();
        assert_eq!(answers[0], answers[1]);
        assert_eq!(answers[0], answers[2]);
    }

    #[test]
    fn each_engine_refuses_the_other_engines_file() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("m.db");
        let log = dir.path().join("m.log");
        AnyEngine::<u64>::open(MetaEngineKind::Sqlite, &db, "test").unwrap().put("k".into(), 1).unwrap();
        AnyEngine::<u64>::open(MetaEngineKind::Bitcask, &log, "test").unwrap().put("k".into(), 1).unwrap();
        let e = AnyEngine::<u64>::open(MetaEngineKind::Bitcask, &db, "test").err().expect("bitcask must refuse a SQLite file");
        assert!(e.to_string().contains("SQLite database"), "{e}");
        assert!(AnyEngine::<u64>::open(MetaEngineKind::Sqlite, &log, "test").and_then(|e| e.put("x".into(), 2)).is_err(), "sqlite must refuse a Bitcask log");
        assert!(AnyEngine::<u64>::open(MetaEngineKind::Rocksdb, &log, "test").is_err(), "rocksdb must refuse a Bitcask log");
        assert!(AnyEngine::<u64>::open(MetaEngineKind::Rocksdb, &db, "test").is_err(), "rocksdb must refuse a SQLite file");
    }
}
