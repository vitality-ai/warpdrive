//! The engine under the S3 metadata rows, chosen at startup.
//!
//! The metadata store is a `WarpMetaStore`. Its engine is either the KV
//! engine (`BitcaskEngine`, from `warp-meta-store`, the default) or the SQL
//! engine (`SqliteEngine`, from `warp-meta-store-sqlite`), picked with
//! `WARPDRIVE_META_ENGINE=bitcask|sqlite`. Both give the same ordering and
//! answers. Only where the bytes live changes.
//!
//! `Engine` has generic methods, so it can't be a trait object. `AnyEngine`
//! is the enum that holds whichever engine was picked and forwards to it.

use serde::de::DeserializeOwned;
use serde::Serialize;
use std::io;
use std::path::Path;
use warp_meta_store::{BitcaskEngine, Engine};
use warp_meta_store_sqlite::SqliteEngine;

/// Which engine stores the metadata rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetaEngineKind {
    /// Append-only log plus an in-memory index. The default.
    Bitcask,
    /// One SQLite table.
    Sqlite,
}

impl MetaEngineKind {
    /// `WARPDRIVE_META_ENGINE`, `bitcask` when unset.
    pub fn from_env() -> Result<Self, String> {
        match std::env::var("WARPDRIVE_META_ENGINE") {
            Err(_) => Ok(Self::Bitcask),
            Ok(v) => Self::parse(&v),
        }
    }

    pub fn parse(v: &str) -> Result<Self, String> {
        match v.trim().to_ascii_lowercase().as_str() {
            "" | "bitcask" | "kv" => Ok(Self::Bitcask),
            "sqlite" | "sql" => Ok(Self::Sqlite),
            other => Err(format!("WARPDRIVE_META_ENGINE={other:?}: expected bitcask or sqlite")),
        }
    }

    /// The file the rows live in when `WARPDRIVE_META_LOG` doesn't name one.
    pub fn default_path(self) -> &'static str {
        match self {
            Self::Bitcask => "cluster_metadata.log",
            Self::Sqlite => "cluster_metadata.db",
        }
    }
}

/// The engine picked at startup.
pub enum AnyEngine<V> {
    Bitcask(BitcaskEngine<V>),
    Sqlite(SqliteEngine<V>),
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
        }
    }

    pub fn kind(&self) -> MetaEngineKind {
        match self {
            Self::Bitcask(_) => MetaEngineKind::Bitcask,
            Self::Sqlite(_) => MetaEngineKind::Sqlite,
        }
    }
}

macro_rules! forward {
    ($self:ident, $e:ident => $call:expr) => {
        match $self {
            AnyEngine::Bitcask($e) => $call,
            AnyEngine::Sqlite($e) => $call,
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
        assert!(MetaEngineKind::parse("rocksdb").is_err());
    }

    #[test]
    fn both_engines_answer_the_same() {
        let dir = tempfile::tempdir().unwrap();
        let engines: Vec<AnyEngine<u64>> = vec![
            AnyEngine::open(MetaEngineKind::Bitcask, dir.path().join("m.log"), "test").unwrap(),
            AnyEngine::open(MetaEngineKind::Sqlite, dir.path().join("m.db"), "test").unwrap(),
        ];
        for e in &engines {
            e.put("a/1".into(), 1).unwrap();
            e.put("a/2".into(), 2).unwrap();
            e.put("b".into(), 3).unwrap();
            e.delete("b").unwrap();
        }
        let answers: Vec<_> = engines.iter().map(|e| (e.snapshot(), e.floor("a/", "a/9", |_| true), e.scan_prefix("a/", |k, v| Some((k.to_string(), *v))))).collect();
        assert_eq!(answers[0], answers[1]);
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
    }
}
