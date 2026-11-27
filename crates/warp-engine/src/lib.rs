//! WarpDrive's distributed engine, as the layers above see it.
//!
//! The engine moves bytes and keeps them correct. It knows nothing about
//! objects, buckets, versions or any protocol. What it offers:
//!
//! - **Rows** ([`RowStore`]): keys with opaque payloads, replicated to every
//!   node. Each row write carries a `seq` from the hybrid logical clock, and
//!   a node applies it only if it's newer than what the key holds, so every
//!   node ends on the same row whatever order writes arrive in. A write
//!   returns once a write quorum has it durably. Deletes leave tombstones.
//!   Layers above can [watch](RowStore::watch) a key prefix to hear about
//!   every write that lands on this node, local or replicated.
//! - **Blobs**: bytes erasure-coded across nodes (coming in this crate next).
//!
//! What it's judged on: bytes moved and syncs per operation, and the
//! ordering and durability above. The object layer builds buckets,
//! versions and the rest out of rows and blobs.

use base64::Engine as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// A row's value: bytes the engine stores and replicates without reading.
///
/// Raw in a binary format (MessagePack), base64 in a text one (JSON logs
/// and the HTTP wire between nodes).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Payload(pub Vec<u8>);

impl Serialize for Payload {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if s.is_human_readable() {
            s.serialize_str(&base64::engine::general_purpose::STANDARD.encode(&self.0))
        } else {
            s.serialize_bytes(&self.0)
        }
    }
}

impl<'de> Deserialize<'de> for Payload {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = Payload;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("row payload (base64 text or raw bytes)")
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Payload, E> {
                base64::engine::general_purpose::STANDARD.decode(v).map(Payload).map_err(E::custom)
            }
            fn visit_bytes<E: serde::de::Error>(self, v: &[u8]) -> Result<Payload, E> {
                Ok(Payload(v.to_vec()))
            }
            fn visit_byte_buf<E: serde::de::Error>(self, v: Vec<u8>) -> Result<Payload, E> {
                Ok(Payload(v))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut a: A) -> Result<Payload, A::Error> {
                let mut out = Vec::with_capacity(a.size_hint().unwrap_or(0));
                while let Some(b) = a.next_element::<u8>()? {
                    out.push(b);
                }
                Ok(Payload(out))
            }
        }
        if d.is_human_readable() { d.deserialize_str(V) } else { d.deserialize_bytes(V) }
    }
}

/// One row: its place in the key's write order and its payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Row {
    pub seq: u64,
    pub payload: Payload,
}

impl warp_meta_store::Record for Row {
    fn seq(&self) -> u64 {
        self.seq
    }
    fn version(&self) -> &str {
        ""
    }
}

/// A row write's future. `Send`, so callers can await it from any runtime.
pub type WriteFuture = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'static>>;

/// Called with a row's key after a write to it lands on this node.
pub type Watcher = Arc<dyn Fn(&str) + Send + Sync>;

/// Replicated, ordered, durable rows with opaque payloads.
///
/// Reads are local: every node holds every row.
pub trait RowStore: Send + Sync {
    /// The live row at `key`.
    fn get(&self, key: &str) -> Option<Row>;

    /// The live row with the greatest key at most `key`, among keys that
    /// start with `prefix`.
    fn floor(&self, prefix: &str, key: &str) -> Option<(String, Row)>;

    /// `f` over every live row whose key starts with `prefix`, in key order.
    fn scan(&self, prefix: &str, f: &mut dyn FnMut(&str, &Row));

    /// Writes `payload` at `key` (a tombstone when `None`), stamped `seq`.
    /// Returns once a write quorum holds it durably.
    fn write(&self, key: &str, seq: u64, payload: Option<Payload>) -> Result<(), String>;

    /// `write` without blocking the caller's thread on the quorum round trip.
    fn write_async(&self, key: String, seq: u64, payload: Option<Payload>) -> WriteFuture;

    /// Calls `on_write` after every write that lands on this node for a key
    /// starting with `prefix`: this node's own writes and those replicated
    /// from peers. A cache over rows uses it to stay correct.
    fn watch(&self, prefix: &str, on_write: Watcher);
}

/// A new `seq` for a row write: the hybrid logical clock, increasing on
/// this node and ahead of every `seq` it has seen.
pub fn next_seq() -> u64 {
    warp_meta_store::hlc::now()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_is_raw_in_messagepack_and_base64_in_json() {
        let row = Row { seq: 7, payload: Payload(vec![0, 1, 2, 255]) };
        let json = serde_json::to_string(&row).unwrap();
        assert_eq!(json, r#"{"seq":7,"payload":"AAEC/w=="}"#);
        assert_eq!(serde_json::from_str::<Row>(&json).unwrap(), row);
        let mp = rmp_serde::to_vec_named(&row).unwrap();
        assert!(mp.windows(4).any(|w| w == [0, 1, 2, 255]), "raw bytes in MessagePack");
        assert_eq!(rmp_serde::from_slice::<Row>(&mp).unwrap(), row);
    }

    #[test]
    fn a_row_reads_its_head_without_its_payload() {
        // The store checks a write against the stored row's head only
        // (`EntryHead`): a row's `seq` is a field named `seq`.
        use warp_meta_store::{Entry, EntryHead};
        let e = Entry::Live(Row { seq: 9, payload: Payload(vec![1; 1000]) });
        let mp = rmp_serde::to_vec_named(&e).unwrap();
        let head: EntryHead = rmp_serde::from_slice(&mp).unwrap();
        assert_eq!(head.seq(), 9);
    }
}
