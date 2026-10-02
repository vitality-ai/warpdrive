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

use super::content_location_store::{ContentDependentRecord, StripeRecord, UnitMeta};
use super::coordinator::{required_write_acks, ClusterState};
use super::packing::Stripe;

/// Also used by `pushdown.rs`'s peer-local filter handler, which needs to
/// read exactly the same shard key a stripe's bins were stored under.
pub(crate) fn stripe_key(key: &str, stripe_index: usize) -> String {
    format!("{key}__cdstripe{stripe_index}")
}

/// `stripes` is computed by the caller (`coordinator.rs`'s dispatch), which
/// needs the same pack result to decide whether this bucket's overhead
/// threshold is satisfied *before* committing to this path — so packing
/// happens exactly once per PUT, not once for the threshold check and
/// again here.
pub async fn put_object_content_dependent(
    bucket: &str,
    key: &str,
    body: &[u8],
    units_header: &[UnitMeta],
    stripes: Vec<Stripe>,
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

    let required_acks = required_write_acks(k, m);
    let mut stripe_records = Vec::with_capacity(stripes.len());

    for (stripe_index, stripe) in stripes.iter().enumerate() {
        let mut bins_bytes: Vec<Vec<u8>> = Vec::with_capacity(k);
        for bin_unit_ids in &stripe.bins {
            let mut buf = Vec::with_capacity(stripe.capacity);
            for uid in bin_unit_ids {
                let unit = units_header
                    .iter()
                    .find(|u| &u.unit_id == uid)
                    .ok_or_else(|| ErrorInternalServerError("packer returned an unknown unit id"))?;
                let (start, end) = (unit.offset as usize, (unit.offset + unit.len) as usize);
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

    let record = ContentDependentRecord {
        bucket: bucket.to_string(),
        key: key.to_string(),
        k,
        m,
        original_len: body.len(),
        units: units_header.to_vec(),
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

/// Fetches and EC-decodes exactly one stripe's `k` data bins — the unit of
/// work both the whole-object GET and the Range-GET share. Only this one
/// stripe's peers are contacted; a Range request that only touches a
/// handful of units never fans out to peers holding unrelated stripes.
async fn fetch_and_decode_stripe(
    record: &ContentDependentRecord,
    stripe_index: usize,
    stripe: &StripeRecord,
    state: &ClusterState,
) -> Result<Vec<Vec<u8>>, Error> {
    let sk = stripe_key(&record.key, stripe_index);

    let t_fetch_start = std::time::Instant::now();
    let gets = stripe.shard_peers.iter().cloned().enumerate().map(|(idx, peer)| {
        let client = Arc::clone(&state.peer_client);
        let bucket = record.bucket.clone();
        let sk = sk.clone();
        async move { client.get_shard(&peer, &bucket, &sk, idx).await.ok().map(|d| (idx, d)) }
    });
    let results = join_all(gets).await;
    let fetch_ms = t_fetch_start.elapsed().as_secs_f64() * 1000.0;

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

    let t_decode_start = std::time::Instant::now();
    let decoded = state.ec.decode_shards(&shards).map_err(|e| ErrorInternalServerError(e.to_string()));
    let decode_ms = t_decode_start.elapsed().as_secs_f64() * 1000.0;
    log::info!("stripe {stripe_index}: fetch={fetch_ms:.2}ms decode={decode_ms:.2}ms shard_count={}", stripe.shard_peers.len());
    decoded
}

pub async fn get_object_content_dependent(record: ContentDependentRecord, state: &ClusterState) -> Result<HttpResponse, Error> {
    let mut output = vec![0u8; record.original_len];
    let units_by_id: HashMap<&str, (u64, u64)> =
        record.units.iter().map(|u| (u.unit_id.as_str(), (u.offset, u.len))).collect();

    for (stripe_index, stripe) in record.stripes.iter().enumerate() {
        let bins = fetch_and_decode_stripe(&record, stripe_index, stripe, state).await?;

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

/// Range-GET for a content-dependent object: `start`/`end` are inclusive
/// byte offsets into the *original* object. Only fetches stripes that hold
/// at least one unit overlapping the requested range — the entire point of
/// doing this at the content-dependent layer instead of just slicing a
/// fully-reconstructed object after the fact (which is what the plain
/// path still does; it has no sub-object structure to be selective about).
/// Returns exactly `end - start + 1` bytes.
pub async fn get_object_content_dependent_range(
    record: ContentDependentRecord,
    start: u64,
    end: u64,
    state: &ClusterState,
) -> Result<Vec<u8>, Error> {
    let want_len = (end - start + 1) as usize;
    let mut output = vec![0u8; want_len];
    let units_by_id: HashMap<&str, &UnitMeta> = record.units.iter().map(|u| (u.unit_id.as_str(), u)).collect();

    let overlaps = |unit: &UnitMeta| unit.offset < end + 1 && unit.offset + unit.len > start;

    let needed_stripes: Vec<usize> = record
        .stripes
        .iter()
        .enumerate()
        .filter(|(_, stripe)| {
            stripe.bins.iter().flatten().any(|uid| {
                units_by_id.get(uid.as_str()).map(|u| overlaps(u)).unwrap_or(false)
            })
        })
        .map(|(i, _)| i)
        .collect();

    log::info!(
        "bucket={} key={} range-GET touched {}/{} stripes",
        record.bucket,
        record.key,
        needed_stripes.len(),
        record.stripes.len()
    );

    // Fetch every needed stripe *concurrently*, not one at a time. A query
    // touching N stripes previously paid N sequential round-trip latencies
    // here — fine when N is 1 (the common case), disastrous on the rarer
    // query whose probed partitions land across many stripes (observed:
    // up to 54 of 95 for a real IVF nprobe search), since that one query
    // would serialize 54 separate peer round-trips end to end. Found by
    // measuring real Lance query latency, not anticipated in advance: a
    // handful of slow outliers were dragging the measured median for
    // ivf_centroid-packed buckets well above the plain path's fixed,
    // single-fetch cost, which this sequential loop made artificially
    // worse than the spatial packing itself ever should have.
    let stripe_fetches = needed_stripes.iter().map(|&stripe_index| {
        let record_ref = &record;
        let stripe = &record.stripes[stripe_index];
        async move {
            fetch_and_decode_stripe(record_ref, stripe_index, stripe, state)
                .await
                .map(|bins| (stripe_index, bins))
        }
    });
    let t_all_stripes = std::time::Instant::now();
    let fetched: Vec<(usize, Vec<Vec<u8>>)> = futures::future::try_join_all(stripe_fetches).await?;
    log::info!(
        "bucket={} key={} all {} stripe(s) fetched+decoded in {:.2}ms (concurrent)",
        record.bucket, record.key, needed_stripes.len(), t_all_stripes.elapsed().as_secs_f64() * 1000.0
    );

    for (stripe_index, bins) in fetched {
        let stripe = &record.stripes[stripe_index];

        for (bin_idx, unit_ids) in stripe.bins.iter().enumerate() {
            let bin_bytes = &bins[bin_idx];
            let mut pos = 0usize;
            for uid in unit_ids {
                let unit = units_by_id
                    .get(uid.as_str())
                    .ok_or_else(|| ErrorInternalServerError("unit id missing from record's unit index"))?;
                let unit_len = unit.len as usize;

                if overlaps(unit) {
                    let i_start = unit.offset.max(start);
                    let i_end = (unit.offset + unit.len).min(end + 1);
                    let src_off = (i_start - unit.offset) as usize;
                    let dst_off = (i_start - start) as usize;
                    let copy_len = (i_end - i_start) as usize;
                    output
                        .get_mut(dst_off..dst_off + copy_len)
                        .ok_or_else(|| ErrorInternalServerError("range slice out of bounds of response buffer"))?
                        .copy_from_slice(
                            bin_bytes
                                .get(pos + src_off..pos + src_off + copy_len)
                                .ok_or_else(|| ErrorInternalServerError("bin too short for its own unit index"))?,
                        );
                }
                pos += unit_len;
            }
        }
    }

    Ok(output)
}
