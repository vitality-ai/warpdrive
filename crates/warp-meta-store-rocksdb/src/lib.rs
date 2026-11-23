//! RocksDB as a WarpMetaStore engine: an [`OrderedKv`] adapter, nothing else.
//!
//! Everything an engine does beyond get, write and scan (encoding,
//! conditional writes, prefix and range scans, floor lookups) lives once in
//! [`warp_meta_store::KvEngine`]. This crate maps those three calls onto
//! RocksDB and chooses its settings:
//!
//! - Durability: every batch is synced to the write-ahead log before it
//!   returns. RocksDB groups concurrent writers into one sync.
//! - Memory: one LRU block cache shared by every RocksDB database in the
//!   process, `WARPDRIVE_ROCKSDB_CACHE_MB` (default 256), holding data, index
//!   and Bloom filter blocks, plus up to two 32 MB write buffers per
//!   database. Memory doesn't grow with the number of keys.
//! - Order: RocksDB's default comparator is bytewise, as `OrderedKv` needs.

use rocksdb::{BlockBasedOptions, Cache, DBCompressionType, Direction, IteratorMode, Options, WriteBatch, WriteOptions, DB};
use std::io;
use std::path::Path;
use std::sync::LazyLock;
use warp_meta_store::OrderedKv;

/// The block cache every RocksDB database in this process shares.
static CACHE: LazyLock<Cache> = LazyLock::new(|| {
    let mb: usize = std::env::var("WARPDRIVE_ROCKSDB_CACHE_MB").ok().and_then(|v| v.trim().parse().ok()).filter(|m| *m > 0).unwrap_or(256);
    Cache::new_lru_cache(mb << 20)
});

/// One RocksDB database as an [`OrderedKv`].
pub struct RocksKv {
    db: DB,
    sync: WriteOptions,
}

fn io_err(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}

impl RocksKv {
    /// Opens (creating if needed) the database in directory `path`.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let mut table = BlockBasedOptions::default();
        table.set_block_cache(&CACHE);
        table.set_bloom_filter(10.0, false);
        // Index and filter blocks count against the cache, so its size bounds
        // them instead of letting them grow with the data.
        table.set_cache_index_and_filter_blocks(true);
        table.set_pin_l0_filter_and_index_blocks_in_cache(true);
        let mut o = Options::default();
        o.create_if_missing(true);
        o.set_block_based_table_factory(&table);
        o.set_max_background_jobs(2);
        o.set_write_buffer_size(32 << 20);
        o.set_max_write_buffer_number(2);
        o.set_compression_type(DBCompressionType::Lz4);
        o.set_max_open_files(256);
        let mut sync = WriteOptions::default();
        sync.set_sync(true);
        let db = DB::open(&o, path.as_ref()).map_err(|e| io_err(format!("{}: {e}", path.as_ref().display())))?;
        Ok(Self { db, sync })
    }
}

impl OrderedKv for RocksKv {
    fn get(&self, key: &[u8]) -> io::Result<Option<Vec<u8>>> {
        self.db.get(key).map_err(io_err)
    }

    fn write(&self, batch: &[(&[u8], Option<&[u8]>)]) -> io::Result<()> {
        let mut b = WriteBatch::default();
        for (k, v) in batch {
            match v {
                Some(v) => b.put(k, v),
                None => b.delete(k),
            }
        }
        self.db.write_opt(b, &self.sync).map_err(io_err)
    }

    fn scan(&self, from: &[u8], descending: bool, f: &mut dyn FnMut(&[u8], &[u8]) -> bool) -> io::Result<()> {
        let dir = if descending { Direction::Reverse } else { Direction::Forward };
        for item in self.db.iterator(IteratorMode::From(from, dir)) {
            let (k, v) = item.map_err(io_err)?;
            if !f(&k, &v) {
                break;
            }
        }
        Ok(())
    }

    fn compact(&self) -> io::Result<()> {
        self.db.compact_range::<&[u8], &[u8]>(None, None);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use warp_meta_store::{assert_behaves_like_bitcask, Applied, Engine, KvEngine, Record, WarpMetaStore};

    fn engine<V>(path: &Path) -> KvEngine<V> {
        KvEngine::new(Box::new(RocksKv::open(path).unwrap()))
    }

    #[test]
    fn behaves_like_the_bitcask_engine() {
        let dir = tempfile::tempdir().unwrap();
        assert_behaves_like_bitcask(&engine::<u64>(&dir.path().join("kv")), dir.path());
    }

    #[test]
    fn writes_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kv");
        {
            let e = engine::<u64>(&path);
            e.put("a".into(), 1).unwrap();
            e.put("b".into(), 2).unwrap();
            e.put("a".into(), 3).unwrap();
            e.delete("b").unwrap();
            e.delete("missing").unwrap();
        }
        let e = engine::<u64>(&path);
        assert_eq!(e.get("a"), Some(3));
        assert_eq!(e.get("b"), None);
        e.compact().unwrap();
        assert_eq!(e.snapshot(), vec![("a".to_string(), 3)]);
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
    fn meta_store_keeps_ordering_and_tombstones_across_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("meta");
        {
            let s = WarpMetaStore::<Rec, _>::new(engine(&path));
            assert!(matches!(s.put("k".into(), rec(10, "new")).unwrap(), Applied::Yes { .. }));
            assert!(matches!(s.put("k".into(), rec(5, "old")).unwrap(), Applied::Stale { current_seq: 10 }), "an older write is refused");
            assert!(matches!(s.delete("k".into(), 20).unwrap(), Applied::Yes { .. }));
        }
        let s = WarpMetaStore::<Rec, _>::new(engine(&path));
        assert_eq!(s.get("k"), None, "the delete survives the restart");
        assert!(matches!(s.put("k".into(), rec(15, "late")).unwrap(), Applied::Stale { current_seq: 20 }), "the tombstone refuses an older late write");
        assert!(matches!(s.put("k".into(), rec(25, "newer")).unwrap(), Applied::Yes { .. }));
        assert_eq!(s.get("k").map(|r| r.v), Some("newer".to_string()));
    }
}
