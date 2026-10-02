//! Content-dependent placement's write/read orchestration: one object
//! becomes `N` independently erasure-coded stripes (via `StripePacker`),
//! not the single-stripe path `coordinator.rs`'s plain `cluster_put_object`
//! uses. Dispatched from there based on the `x-warpd-computable-units`
//! header's presence (PUT) or a `ContentLocationStore` hit (GET) — see
//! docs/Distributed-Engine-Plan.md's phase 3.
//!
use actix_web::error::ErrorInternalServerError;
use actix_web::{Error, HttpResponse};
use futures::future::join_all;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use super::content_location_store::{ContentDependentRecord, StripeRecord};
use super::coordinator::{required_write_acks, ClusterState};
use super::packing::Unit;

fn stripe_key(key: &str, stripe_index: usize) -> String {
    format!("{key}__cdstripe{stripe_index}")
}

pub async fn put_object_content_dependent(
    bucket: &str,
    key: &str,
    body: &[u8],
    units_header: &[(u64, u64)],
    state: &ClusterState,
) -> Result<HttpResponse, Error> {
    let peers = state.membership.peers();
    if peers.is_empty() {
        return Err(ErrorInternalServerError(
            "no peers configured (set WARPDRIVE_PEERS or join the cluster first)",
        ));
    }

    let k = state.ec.k();
    let m = state.ec.m();
    if peers.len() < k + m {
        return Err(ErrorInternalServerError(format!(
            "need at least {} peers for RS({k},{m}), have {}",
            k + m,
            peers.len()
        )));
    }

    let units: Vec<Unit> = units_header
        .iter()
        .enumerate()
        .map(|(i, &(_offset, len))| Unit { unit_id: format!("u{i}"), size: len as usize })
        .collect();

    let stripes = state.packer.pack(k, &units);
    let required_acks = required_write_acks(k, m);
    let mut stripe_records = Vec::with_capacity(stripes.len());

    for (stripe_index, stripe) in stripes.iter().enumerate() {
        let mut bins_bytes: Vec<Vec<u8>> = Vec::with_capacity(k);
        for bin_unit_ids in &stripe.bins {
            let mut buf = Vec::with_capacity(stripe.capacity);
            for uid in bin_unit_ids {
                let idx: usize = uid
                    .strip_prefix('u')
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(|| ErrorInternalServerError("malformed unit id from packer"))?;
                let (offset, len) = units_header[idx];
                let (start, end) = (offset as usize, (offset + len) as usize);
                buf.extend_from_slice(
                    body.get(start..end)
                        .ok_or_else(|| ErrorInternalServerError("computable unit out of bounds of body"))?,
                );
            }
            buf.resize(stripe.capacity, 0u8);
            bins_bytes.push(buf);
        }

        let all_shards = state
            .ec
            .encode_shards(bins_bytes)
            .map_err(|e| ErrorInternalServerError(e.to_string()))?;

        let sk = stripe_key(key, stripe_index);
        let chosen = state.placement.place(bucket, &sk, &peers, k, m);

        let puts = chosen.iter().cloned().zip(all_shards).enumerate().map(|(idx, (peer, shard))| {
            let client = Arc::clone(&state.peer_client);
            let bucket = bucket.to_string();
            let sk = sk.clone();
            async move { client.put_shard(&peer, &bucket, &sk, idx, shard).await }
        });
        let results = join_all(puts).await;
        let acked = results.iter().filter(|r| r.is_ok()).count();
        if acked < required_acks {
            return Err(ErrorInternalServerError(format!(
                "stripe {stripe_index} write quorum not met: {acked}/{required_acks}"
            )));
        }

        stripe_records.push(StripeRecord {
            shard_peers: chosen,
            capacity: stripe.capacity,
            bins: stripe.bins.clone(),
        });
    }

    let units_with_offsets: Vec<(String, u64, u64)> = units_header
        .iter()
        .enumerate()
        .map(|(i, &(o, l))| (format!("u{i}"), o, l))
        .collect();

    let record = ContentDependentRecord {
        bucket: bucket.to_string(),
        key: key.to_string(),
        k,
        m,
        original_len: body.len(),
        units: units_with_offsets,
        stripes: stripe_records,
    };

    // Replicate to the union of every stripe's peers — different stripes
    // can land on different peer subsets (each resolved independently via
    // its own stripe-qualified key), so the pin needs to reach all of
    // them, not just one, for any of them to be able to coordinate a
    // future GET. Same pattern as `coordinator.rs`'s `replicate_location_put`.
    let all_peers: Vec<String> = record
        .stripes
        .iter()
        .flat_map(|s| s.shard_peers.iter().cloned())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();

    let puts = all_peers.iter().cloned().map(|peer| {
        let client = state.location_http.clone();
        let record = record.clone();
        async move {
            let url = format!("{}/cluster/_internal/content_location", peer.trim_end_matches('/'));
            client.post(&url).json(&record).send().await.map(|r| r.status().is_success()).unwrap_or(false)
        }
    });
    let results = join_all(puts).await;
    let acked = results.iter().filter(|ok| **ok).count();
    if acked < required_acks {
        return Err(ErrorInternalServerError(format!(
            "content-location pin quorum not met: {acked}/{required_acks} peers stored the record"
        )));
    }

    Ok(HttpResponse::Ok().finish())
}

pub async fn get_object_content_dependent(record: ContentDependentRecord, state: &ClusterState) -> Result<HttpResponse, Error> {
    let mut output = vec![0u8; record.original_len];
    let units_by_id: HashMap<&str, (u64, u64)> =
        record.units.iter().map(|(id, o, l)| (id.as_str(), (*o, *l))).collect();

    for (stripe_index, stripe) in record.stripes.iter().enumerate() {
        let sk = stripe_key(&record.key, stripe_index);

        let gets = stripe.shard_peers.iter().cloned().enumerate().map(|(idx, peer)| {
            let client = Arc::clone(&state.peer_client);
            let bucket = record.bucket.clone();
            let sk = sk.clone();
            async move { client.get_shard(&peer, &bucket, &sk, idx).await.ok().map(|d| (idx, d)) }
        });
        let results = join_all(gets).await;

        let mut shards: Vec<Option<Vec<u8>>> = vec![None; stripe.shard_peers.len()];
        for (idx, data) in results.into_iter().flatten() {
            shards[idx] = Some(data);
        }
        let present = shards.iter().filter(|s| s.is_some()).count();
        if present < record.k {
            return Err(ErrorInternalServerError(format!(
                "stripe {stripe_index} read quorum not met: {present}/{}",
                record.k
            )));
        }

        let bins = state.ec.decode_shards(&shards).map_err(|e| ErrorInternalServerError(e.to_string()))?;

        for (bin_idx, unit_ids) in stripe.bins.iter().enumerate() {
            let bin_bytes = &bins[bin_idx];
            let mut pos = 0usize;
            for uid in unit_ids {
                let (offset, len) = *units_by_id
                    .get(uid.as_str())
                    .ok_or_else(|| ErrorInternalServerError("unit id missing from record's unit index"))?;
                let len = len as usize;
                let (start, end) = (offset as usize, offset as usize + len);
                output
                    .get_mut(start..end)
                    .ok_or_else(|| ErrorInternalServerError("reconstructed unit out of bounds of original object"))?
                    .copy_from_slice(
                        bin_bytes
                            .get(pos..pos + len)
                            .ok_or_else(|| ErrorInternalServerError("bin too short for its own unit index"))?,
                    );
                pos += len;
            }
        }
    }

    Ok(HttpResponse::Ok().body(output))
}
