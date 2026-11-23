//! WarpMetaStore: WarpDrive's metadata store.
//!
//! - [`WarpMetaStore`]: the store to use. Every record carries a `seq` from the
//!   hybrid logical clock ([`hlc`]), and a write applies only if it is newer
//!   than what the key holds, so every replica ends on the same record
//!   whatever order writes arrive in. Deletes leave versioned tombstones.
//! - [`Engine`]: what a storage engine implements underneath a `WarpMetaStore`.
//!   [`BitcaskEngine`] (an append-only log with an in-memory index) is the
//!   default. Any ordered key-value store becomes an engine through
//!   [`kv::KvEngine`], which builds the whole trait on a small [`OrderedKv`]
//!   interface (get, durable batch write, ordered scan): a new store is only
//!   an adapter for those calls. Use an engine directly only for state that one node writes and
//!   no other node holds a copy of, such as a node's own shard index.
//! - [`durable_log`] and [`group_commit`]: the durable append log the Bitcask
//!   engine writes to, with fsyncs batched across concurrent writers.

pub mod durable_log;
pub mod engine;
pub mod group_commit;
pub mod hlc;
pub mod kv;
pub mod store;

pub use engine::{BitcaskEngine, Engine};
pub use kv::{assert_behaves_like_bitcask, KvEngine, MemKv, OrderedKv};
pub use store::{Applied, Entry, Record, WarpMetaStore};
