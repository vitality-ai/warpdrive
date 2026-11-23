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
    /// A small blob's bytes, base64, kept in the row itself
    /// (`WARPDRIVE_INLINE_MAX_BYTES`, RFC 5 T1).
    #[serde(default)]
    pub inline: Option<String>,
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
            inline: Some(base64::Engine::encode(&b64(), body)),
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
        return base64::Engine::decode(&b64(), data).map_err(|e| ErrorInternalServerError(format!("corrupt inline blob: {e}")));
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
