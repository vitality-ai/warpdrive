//! Writing and reading one blob's shards, with no record of their own.
//!
//! The S3 metadata row of the object a blob belongs to carries its
//! placement (`BlobPlacement`), so a PUT replicates one record, not two.
//! This module only moves shard bytes: encode and write `k+m` shards to a
//! write quorum, or fetch any `k` and decode.

use actix_web::error::ErrorInternalServerError;
use actix_web::Error;
use futures::future::join_all;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use super::coordinator::{required_write_acks, ClusterState};
use super::ec::ErasureCoder;
use super::shard_storage::{new_version_id, versioned_key};

/// Where one blob's bytes are.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BlobPlacement {
    /// Index i = the node holding shard i (data shards first). Empty for an
    /// inline blob.
    pub shard_peers: Vec<String>,
    pub k: usize,
    pub m: usize,
    /// The shards' key on their nodes (`versioned_key`).
    pub wire_key: String,
    /// The blob's length: the object bytes it holds.
    pub len: u64,
    /// When the stored bytes are compressed (`Compression`), their length.
    /// The shards and an inline value hold the compressed bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zstd_len: Option<u64>,
    /// A small blob's bytes, kept in the row itself
    /// (`WARPDRIVE_INLINE_MAX_BYTES`, RFC 5 T1).
    #[serde(default)]
    pub inline: Option<InlineBytes>,
}

/// Inline blob bytes: raw in a binary format (the RocksDB engine's records),
/// base64 in a text one (JSON logs and the HTTP wire between nodes).
#[derive(Debug, Clone, PartialEq)]
pub struct InlineBytes(pub Vec<u8>);

impl Serialize for InlineBytes {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if s.is_human_readable() {
            s.serialize_str(&base64::Engine::encode(&b64(), &self.0))
        } else {
            s.serialize_bytes(&self.0)
        }
    }
}

impl<'de> Deserialize<'de> for InlineBytes {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = InlineBytes;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("inline bytes (base64 text or raw bytes)")
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<InlineBytes, E> {
                base64::Engine::decode(&b64(), v).map(InlineBytes).map_err(E::custom)
            }
            fn visit_bytes<E: serde::de::Error>(self, v: &[u8]) -> Result<InlineBytes, E> {
                Ok(InlineBytes(v.to_vec()))
            }
            fn visit_byte_buf<E: serde::de::Error>(self, v: Vec<u8>) -> Result<InlineBytes, E> {
                Ok(InlineBytes(v))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut a: A) -> Result<InlineBytes, A::Error> {
                let mut out = Vec::with_capacity(a.size_hint().unwrap_or(0));
                while let Some(b) = a.next_element::<u8>()? {
                    out.push(b);
                }
                Ok(InlineBytes(out))
            }
        }
        if d.is_human_readable() { d.deserialize_str(V) } else { d.deserialize_bytes(V) }
    }
}

fn b64() -> base64::engine::general_purpose::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

/// When to compress a blob. A new policy only implements this and is
/// picked in `Compression::from_env`.
pub trait CompressionPolicy: Send + Sync {
    /// For the startup log.
    fn name(&self) -> String;
    /// Whether to try compressing a blob of `size` bytes, which puts
    /// `wire_factor` times its size on the network.
    fn try_compress(&self, size: usize, wire_factor: f64) -> bool;
    /// One attempt: `size` bytes in, `out` bytes out, in `secs`.
    fn observe(&self, _size: usize, _out: usize, _secs: f64) {}
    /// Whether a blob was stored compressed.
    fn stored(&self, _compressed: bool) {}
    /// A blob was read from its shards.
    fn read(&self) {}
    /// What the policy sees, for `/cluster/_internal/timing`.
    fn view(&self) -> Option<serde_json::Value> {
        None
    }
}

/// Compress every blob.
pub struct Always;

impl CompressionPolicy for Always {
    fn name(&self) -> String {
        "every blob".into()
    }
    fn try_compress(&self, _size: usize, _wire_factor: f64) -> bool {
        true
    }
}

/// Blob compression: zstd at `level`, when `policy` says so. Set once at
/// startup from `WARPDRIVE_BLOB_COMPRESSION`: unset or `off`, `zstd[:level]`
/// (every blob) or `auto[:level]` (`adaptive`). A blob is kept compressed
/// only if that saves at least a tenth of it, so incompressible data costs
/// one failed attempt.
#[derive(Clone)]
pub struct Compression {
    level: i32,
    policy: Option<Arc<dyn CompressionPolicy>>,
}

/// Blobs smaller than this aren't worth compressing.
const COMPRESS_MIN: usize = 512;

impl Compression {
    pub fn off() -> Self {
        Self { level: 1, policy: None }
    }

    pub fn with(level: i32, policy: Arc<dyn CompressionPolicy>) -> Self {
        Self { level, policy: Some(policy) }
    }

    pub fn from_env() -> Self {
        let v = std::env::var("WARPDRIVE_BLOB_COMPRESSION").unwrap_or_default().trim().to_ascii_lowercase();
        let level = |rest: &str| rest.strip_prefix(':').and_then(|l| l.trim().parse().ok()).unwrap_or(1);
        if let Some(rest) = v.strip_prefix("zstd") {
            Self::with(level(rest), Arc::new(Always))
        } else if let Some(rest) = v.strip_prefix("auto") {
            Self::with(level(rest), Arc::new(super::adaptive::Adaptive::new(super::adaptive::NodeLoad::start())))
        } else {
            Self::off()
        }
    }

    pub fn describe(&self) -> Option<String> {
        self.policy.as_ref().map(|p| format!("with zstd level {}, {}", self.level, p.name()))
    }

    pub fn view(&self) -> Option<serde_json::Value> {
        self.policy.as_ref().and_then(|p| p.view())
    }

    fn read(&self) {
        if let Some(p) = &self.policy {
            p.read();
        }
    }

    /// The bytes to store for `body`, and their length when compressed.
    /// `wire_factor` is how many times its size a blob puts on the network.
    fn apply<'a>(&self, body: &'a [u8], wire_factor: f64) -> (std::borrow::Cow<'a, [u8]>, Option<u64>) {
        let Some(policy) = &self.policy else { return (body.into(), None) };
        if body.len() < COMPRESS_MIN || !policy.try_compress(body.len(), wire_factor) {
            policy.stored(false);
            return (body.into(), None);
        }
        let t0 = std::time::Instant::now();
        let out = zstd::bulk::compress(body, self.level);
        if let Ok(c) = &out {
            policy.observe(body.len(), c.len(), t0.elapsed().as_secs_f64());
        }
        match out {
            Ok(c) if c.len() <= body.len() - body.len() / 10 => {
                policy.stored(true);
                let n = c.len() as u64;
                (c.into(), Some(n))
            }
            _ => {
                policy.stored(false);
                (body.into(), None)
            }
        }
    }
}

/// The object bytes from a blob's stored bytes.
fn unpack(p: &BlobPlacement, stored: Vec<u8>) -> Result<Vec<u8>, Error> {
    match p.zstd_len {
        None => Ok(stored),
        Some(_) => zstd::bulk::decompress(&stored, p.len as usize).map_err(|e| ErrorInternalServerError(format!("corrupt compressed blob: {e}"))),
    }
}

/// Encodes `body` and writes its shards under `key` in `bucket`, returning
/// once a write quorum holds them. Small bodies are returned inline
/// instead, with no shard round.
pub async fn write_blob(state: &ClusterState, user_id: &str, bucket: &str, key: &str, body: &[u8]) -> Result<BlobPlacement, Error> {
    let (k, m) = (state.ec.k(), state.ec.m());
    let raw_len = body.len() as u64;
    let max_inline = super::coordinator::inline_max_bytes();
    // Whether a blob is inline is decided on its own size, compressed or not.
    // Inline values go to every node, shards to k+m of them.
    let wire_factor = if max_inline > 0 && body.len() <= max_inline { state.membership.peers().len().max(1) as f64 } else { (k + m) as f64 / k as f64 };
    let (stored, zstd_len) = state.compression.apply(body, wire_factor);
    if max_inline > 0 && body.len() <= max_inline {
        return Ok(BlobPlacement {
            shard_peers: Vec::new(),
            k,
            m,
            wire_key: String::new(),
            len: body.len() as u64,
            zstd_len,
            inline: Some(InlineBytes(stored.into_owned())),
        });
    }
    let body: &[u8] = &stored;

    let peers = state.membership.peers();
    if peers.len() < k + m {
        return Err(ErrorInternalServerError(format!("need at least {} peers for RS({k},{m}), have {}", k + m, peers.len())));
    }
    let shard_len = body.len().div_ceil(k).max(1);
    if let Some(max) = state.peer_client.max_shard_size() {
        if shard_len > max {
            return Err(ErrorInternalServerError(format!(
                "blob too large for RS({k},{m}): each shard would be {shard_len} bytes, over this transport's {max}-byte limit"
            )));
        }
    }

    let chosen = state.placement.place(bucket, key, &peers, k, m);
    let encoded = state.ec.encode(body).map_err(|e| ErrorInternalServerError(e.to_string()))?;
    // A fresh version per write (#151): a retried or racing write never
    // lands on shards another placement still points at.
    let wire_key = versioned_key(key, &new_version_id());
    let puts = chosen.iter().cloned().zip(encoded.shards).enumerate().map(|(idx, (peer, shard))| {
        let client = Arc::clone(&state.peer_client);
        let (bucket, wire_key, user) = (bucket.to_string(), wire_key.clone(), user_id.to_string());
        async move {
            let res = client.put_shard(&peer, &user, &bucket, &wire_key, idx, shard).await;
            if let Err(e) = &res {
                log::warn!("put_shard idx={idx} peer={peer} failed: {e}");
            }
            res.is_ok()
        }
    });
    let acked = join_all(puts).await.into_iter().filter(|ok| *ok).count();
    let required = required_write_acks(k, m);
    if acked < required {
        return Err(ErrorInternalServerError(format!("write quorum not met: {acked}/{required} nodes stored a shard (RS({k},{m}))")));
    }
    Ok(BlobPlacement { shard_peers: chosen, k, m, wire_key, len: raw_len, zstd_len, inline: None })
}

/// Reads a blob back: inline bytes, or any `k` shards decoded.
pub async fn read_blob(state: &ClusterState, user_id: &str, bucket: &str, p: &BlobPlacement) -> Result<Vec<u8>, Error> {
    if let Some(data) = &p.inline {
        return unpack(p, data.0.clone());
    }
    state.compression.read();
    let gets = p.shard_peers.iter().cloned().enumerate().map(|(idx, peer)| {
        let client = Arc::clone(&state.peer_client);
        let (bucket, wire_key, user) = (bucket.to_string(), p.wire_key.clone(), user_id.to_string());
        async move { client.get_shard(&peer, &user, &bucket, &wire_key, idx).await.ok().map(|d| (idx, d)) }
    });
    let mut shards: Vec<Option<Vec<u8>>> = vec![None; p.shard_peers.len()];
    for (idx, data) in join_all(gets).await.into_iter().flatten() {
        shards[idx] = Some(data);
    }
    let present = shards.iter().filter(|s| s.is_some()).count();
    if present < p.k {
        return Err(ErrorInternalServerError(format!("read quorum not met: {present}/{} shards available", p.k)));
    }
    let decoder = super::ec::ReedSolomonCoder::new(p.k, p.m).map_err(|e| ErrorInternalServerError(e.to_string()))?;
    let stored = decoder.decode(&shards, p.zstd_len.unwrap_or(p.len) as usize).map_err(|e| ErrorInternalServerError(e.to_string()))?;
    unpack(p, stored)
}

#[cfg(test)]
mod compression_tests {
    use super::*;

    fn placement(len: usize, zstd_len: Option<u64>) -> BlobPlacement {
        BlobPlacement { shard_peers: vec![], k: 1, m: 0, wire_key: String::new(), len: len as u64, zstd_len, inline: None }
    }

    #[test]
    fn compressible_blobs_shrink_and_come_back_exactly() {
        let text: Vec<u8> = (0..100_000u32).flat_map(|i| format!("{{\"id\":{i},\"name\":\"user{}\"}}\n", i % 97).into_bytes()).collect();
        let (stored, zlen) = Compression::with(1, Arc::new(Always)).apply(&text, 1.0);
        let zlen = zlen.expect("text compresses");
        assert!(zlen * 3 < text.len() as u64, "{zlen} of {}", text.len());
        assert_eq!(unpack(&placement(text.len(), Some(zlen)), stored.into_owned()).unwrap(), text);
    }

    #[test]
    fn incompressible_and_small_blobs_are_stored_as_they_are() {
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let random: Vec<u8> = (0..65_536).map(|_| { x ^= x << 13; x ^= x >> 7; x ^= x << 17; x as u8 }).collect();
        assert_eq!(Compression::with(1, Arc::new(Always)).apply(&random, 1.0).1, None, "random bytes don't shrink by a tenth");
        assert_eq!(Compression::with(1, Arc::new(Always)).apply(&[0u8; 100], 1.0).1, None, "below COMPRESS_MIN");
        assert_eq!(Compression::off().apply(&[0u8; 100_000], 1.0).1, None, "off");
        assert_eq!(unpack(&placement(4, None), b"abcd".to_vec()).unwrap(), b"abcd");
    }
}
