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
    pub len: u64,
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

/// Encodes `body` and writes its shards under `key` in `bucket`, returning
/// once a write quorum holds them. Small bodies are returned inline
/// instead, with no shard round.
pub async fn write_blob(state: &ClusterState, user_id: &str, bucket: &str, key: &str, body: &[u8]) -> Result<BlobPlacement, Error> {
    let (k, m) = (state.ec.k(), state.ec.m());
    let max_inline = super::coordinator::inline_max_bytes();
    if max_inline > 0 && body.len() <= max_inline {
        return Ok(BlobPlacement {
            shard_peers: Vec::new(),
            k,
            m,
            wire_key: String::new(),
            len: body.len() as u64,
            inline: Some(InlineBytes(body.to_vec())),
        });
    }

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
    Ok(BlobPlacement { shard_peers: chosen, k, m, wire_key, len: body.len() as u64, inline: None })
}

/// Reads a blob back: inline bytes, or any `k` shards decoded.
pub async fn read_blob(state: &ClusterState, user_id: &str, bucket: &str, p: &BlobPlacement) -> Result<Vec<u8>, Error> {
    if let Some(data) = &p.inline {
        return Ok(data.0.clone());
    }
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
    decoder.decode(&shards, p.len as usize).map_err(|e| ErrorInternalServerError(e.to_string()))
}
