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
use std::collections::HashMap;
use std::sync::Arc;

use super::content_location_store::{ContentDependentRecord, StripeRecord, UnitMeta};
use super::coordinator::{replicate_location_delete, required_write_acks, ClusterState};
use super::ec::ErasureCoder;
use super::packing::Stripe;

/// Generic, packer-agnostic correctness check: for every unit the caller
/// declared in `units_header`, confirms it was packed into exactly one bin
/// (not zero -- dropped; not more than one -- duplicated) and that the
/// bytes that bin holds for it match the original object exactly. Any
/// `StripePacker` bug -- a dropped unit, a duplicated unit, a unit
/// assigned to the wrong offset -- corrupts the reconstructed object the
/// same way regardless of which packer produced it, so one mechanism
/// catches all of them, without being a bespoke check of any one
/// algorithm's own logic. `unpadded_bins` is each stripe's bins *before*
/// padding to `stripe.capacity`, i.e. exactly the concatenated unit bytes
/// that went in.
///
/// Deliberately checks per-*unit* occurrence, not whole-*body* byte
/// coverage: an earlier version required every single byte of the
/// original object to be covered by some unit, which is wrong whenever a
/// caller's own `units_header` is legitimately sparse (e.g. row groups
/// only, no header/footer framing unit) -- that's the caller's choice,
/// not a packer bug, and the read path already tolerates it by zero-
/// filling gaps. This version only requires that whatever units the
/// caller *did* declare are each packed exactly once, correctly. As a
/// side effect this also catches two declared units whose byte ranges
/// overlap each other landing in different bins with conflicting content
/// (the previous coverage-bitmap version silently accepted that), and
/// it's O(units) extra memory instead of O(body bytes) for the tracking
/// structure, which matters once an object has a several-hundred-MB body
/// but only a few hundred units (a real Parquet file, see
/// parquet_real_offsets.py).
fn verify_stripes_reconstruct_original(
    body: &[u8],
    units_header: &[UnitMeta],
    stripes: &[Stripe],
    unpadded_bins: &[Vec<Vec<u8>>],
) -> Result<(), Error> {
    let units_by_id: HashMap<&str, &UnitMeta> =
        units_header.iter().map(|u| (u.unit_id.as_str(), u)).collect();
    let mut occurrences: HashMap<&str, u32> = units_header.iter().map(|u| (u.unit_id.as_str(), 0)).collect();

    for (stripe, bins) in stripes.iter().zip(unpadded_bins.iter()) {
        for (unit_ids, bin_bytes) in stripe.bins.iter().zip(bins.iter()) {
            let mut pos = 0usize;
            for uid in unit_ids {
                let unit = *units_by_id
                    .get(uid.as_str())
                    .ok_or_else(|| ErrorInternalServerError("packer returned an unknown unit id"))?;
                let (start, len) = (unit.offset as usize, unit.len as usize);
                let end = start + len;
                let original = body
                    .get(start..end)
                    .ok_or_else(|| ErrorInternalServerError("packed unit out of bounds of the original object"))?;
                let packed = bin_bytes
                    .get(pos..pos + len)
                    .ok_or_else(|| ErrorInternalServerError("bin too short for its own unit index"))?;
                if original != packed {
                    return Err(ErrorInternalServerError(
                        "content-dependent checksum failed: a packed unit's bytes do not match the original object",
                    ));
                }
                *occurrences.get_mut(uid.as_str()).unwrap() += 1;
                pos += len;
            }
        }
    }

    if let Some((bad_id, &count)) = occurrences.iter().find(|(_, &count)| count != 1) {
        return Err(ErrorInternalServerError(format!(
            "content-dependent checksum failed: unit {bad_id} appears {count} times across the packer's stripes (expected exactly 1)"
        )));
    }
    Ok(())
}

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

    // Built once, not re-scanned per unit (#161): with a few hundred to a
    // few thousand units (a real Parquet file's column chunks, or an IVF
    // index's partitions), an `.iter().find()` per unit inside these
    // nested loops is O(units) work for *each* unit, O(units^2) overall.
    let units_by_id: HashMap<&str, &UnitMeta> =
        units_header.iter().map(|u| (u.unit_id.as_str(), u)).collect();

    // Build every stripe's bins *unpadded* first (the exact bytes a GET's
    // reassembly expects to get back out), and verify the whole set
    // reconstructs `body` byte-for-byte before any network call happens —
    // see `verify_stripes_reconstruct_original`'s doc for why this is
    // generic (packer-agnostic), not a bespoke check of one algorithm.
    let mut unpadded_bins: Vec<Vec<Vec<u8>>> = Vec::with_capacity(stripes.len());
    for stripe in &stripes {
        let mut bins_bytes: Vec<Vec<u8>> = Vec::with_capacity(stripe.bins.len());
        for bin_unit_ids in &stripe.bins {
            let mut buf = Vec::new();
            for uid in bin_unit_ids {
                let unit = *units_by_id
                    .get(uid.as_str())
                    .ok_or_else(|| ErrorInternalServerError("packer returned an unknown unit id"))?;
                let (start, end) = (unit.offset as usize, (unit.offset + unit.len) as usize);
                buf.extend_from_slice(
                    body.get(start..end)
                        .ok_or_else(|| ErrorInternalServerError("computable unit out of bounds of body"))?,
                );
            }
            bins_bytes.push(buf);
        }
        unpadded_bins.push(bins_bytes);
    }
    verify_stripes_reconstruct_original(body, units_header, &stripes, &unpadded_bins)?;

    let mut stripe_records = Vec::with_capacity(stripes.len());

    for (stripe_index, (stripe, bins_bytes)) in stripes.iter().zip(unpadded_bins.into_iter()).enumerate() {
        let mut bins_bytes = bins_bytes;
        for buf in bins_bytes.iter_mut() {
            buf.resize(stripe.capacity, 0u8);
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

    // Replicate to *every* known peer, not just the union of this object's
    // stripe peers — same reasoning as `coordinator.rs`'s
    // `replicate_location_put`: a GET can land on any node, and only a node
    // holding a copy of this pin can find the object at all.
    let puts = peers.iter().cloned().map(|peer| {
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

    // Symmetric case to `put_object_plain`'s stale-packed-record cleanup,
    // and the same unconditional-broadcast fix: this *coordinator* only
    // needed quorum to receive the original plain pin, so it may have no
    // local copy even though other peers still do. Don't gate the
    // tombstone broadcast on a local `.is_some()` check (see issue #153).
    let _ = state.location_store.delete(bucket, key);
    let cleanup_acked = replicate_location_delete(state, bucket, key, &peers).await;
    if cleanup_acked < required_acks {
        return Err(ErrorInternalServerError(format!(
            "wrote the new object, but could not confirm the stale plain pin was cleared cluster-wide: \
             {cleanup_acked}/{required_acks} peers acknowledged the tombstone (a GET on an unacknowledged \
             peer may still return the old plain bytes)"
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

    // Same fix as `coordinator.rs`'s plain GET: decode with *this record's*
    // k/m, not the node's process-wide `state.ec`.
    let t_decode_start = std::time::Instant::now();
    let decoded = super::ec::ReedSolomonCoder::new(record.k, record.m)
        .and_then(|decoder| decoder.decode_shards(&shards))
        .map_err(|e| ErrorInternalServerError(e.to_string()));
    let decode_ms = t_decode_start.elapsed().as_secs_f64() * 1000.0;
    log::info!("stripe {stripe_index}: fetch={fetch_ms:.2}ms decode={decode_ms:.2}ms shard_count={}", stripe.shard_peers.len());
    decoded
}

pub async fn get_object_content_dependent(record: ContentDependentRecord, state: &ClusterState) -> Result<HttpResponse, Error> {
    let mut output = vec![0u8; record.original_len];
    let units_by_id: HashMap<&str, (u64, u64)> =
        record.units.iter().map(|u| (u.unit_id.as_str(), (u.offset, u.len))).collect();

    // Fetch every stripe concurrently, not one at a time: the Range-GET
    // path below already learned this lesson (see its own comment, a
    // real IVF query touched up to 54 of 95 stripes and paid 54
    // sequential round trips before being fixed). A whole-object GET
    // touches *every* stripe by definition, so it pays this cost on
    // every single request, not just a rare wide query, making this the
    // more important of the two paths to fix.
    let stripe_fetches = record.stripes.iter().enumerate().map(|(stripe_index, stripe)| {
        let record_ref = &record;
        async move {
            fetch_and_decode_stripe(record_ref, stripe_index, stripe, state)
                .await
                .map(|bins| (stripe_index, bins))
        }
    });
    let fetched: Vec<(usize, Vec<Vec<u8>>)> = futures::future::try_join_all(stripe_fetches).await?;

    for (stripe_index, bins) in fetched {
        let stripe = &record.stripes[stripe_index];

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

#[cfg(test)]
mod checksum_tests {
    use super::*;

    fn unit(id: &str, offset: u64, len: u64) -> UnitMeta {
        UnitMeta { unit_id: id.to_string(), offset, len, uncompressed_len: len, codec: "opaque".to_string(), metadata: Vec::new() }
    }

    // One stripe, 2 bins: bin0 = [u0], bin1 = [u1]. Matches how
    // `put_object_content_dependent` actually shapes `unpadded_bins` (one
    // inner Vec<u8> per bin, in `stripe.bins` order), before padding.
    fn stripe_two_bins() -> Stripe {
        Stripe { bins: vec![vec!["u0".to_string()], vec!["u1".to_string()]], capacity: 5 }
    }

    #[test]
    fn verify_passes_when_stripes_reconstruct_the_original_exactly() {
        let body = b"helloworld".to_vec(); // u0="hello" (0..5), u1="world" (5..10)
        let units = vec![unit("u0", 0, 5), unit("u1", 5, 5)];
        let stripes = vec![stripe_two_bins()];
        let bins = vec![vec![b"hello".to_vec(), b"world".to_vec()]];

        assert!(verify_stripes_reconstruct_original(&body, &units, &stripes, &bins).is_ok());
    }

    #[test]
    fn verify_fails_when_a_unit_is_dropped_entirely() {
        let body = b"helloworld".to_vec();
        let units = vec![unit("u0", 0, 5), unit("u1", 5, 5)];
        let stripes = vec![stripe_two_bins()];
        // bin1 is empty: u1's bytes never get written anywhere -- the
        // "dropped unit" bug class this check exists to catch.
        let bins = vec![vec![b"hello".to_vec(), Vec::new()]];

        let err = verify_stripes_reconstruct_original(&body, &units, &stripes, &bins);
        assert!(err.is_err());
    }

    #[test]
    fn verify_fails_when_bin_bytes_dont_match_the_original() {
        let body = b"helloworld".to_vec();
        let units = vec![unit("u0", 0, 5), unit("u1", 5, 5)];
        let stripes = vec![stripe_two_bins()];
        // Full coverage, but u1's bytes are simply wrong (e.g. a corrupted
        // or mis-assigned bin) -- the coverage check alone wouldn't catch
        // this, only the byte-equality check does.
        let bins = vec![vec![b"hello".to_vec(), b"WORLD".to_vec()]];

        let err = verify_stripes_reconstruct_original(&body, &units, &stripes, &bins);
        assert!(err.is_err());
    }

    #[test]
    fn verify_allows_a_sparse_units_header_that_does_not_cover_the_whole_body() {
        // body is 10 bytes, but the caller only declared a unit for the
        // first 5 -- legitimate (row groups only, no header/footer framing
        // unit), not a packer bug. The read path already zero-fills the
        // uncovered tail; this check must not reject it.
        let body = b"helloworld".to_vec();
        let units = vec![unit("u0", 0, 5)];
        let stripes = vec![Stripe { bins: vec![vec!["u0".to_string()]], capacity: 5 }];
        let bins = vec![vec![b"hello".to_vec()]];

        assert!(verify_stripes_reconstruct_original(&body, &units, &stripes, &bins).is_ok());
    }

    #[test]
    fn verify_fails_when_a_unit_is_packed_into_more_than_one_bin() {
        let body = b"helloworld".to_vec();
        let units = vec![unit("u0", 0, 5), unit("u1", 5, 5)];
        // u0 duplicated into both bins of a two-bin stripe; u1 never
        // packed at all. Byte content is "correct" everywhere it was
        // written, so a whole-body coverage/equality check alone can miss
        // this -- only counting occurrences per declared unit catches it.
        let stripes = vec![stripe_two_bins()];
        let bins = vec![vec![b"hello".to_vec(), b"hello".to_vec()]];

        let err = verify_stripes_reconstruct_original(&body, &units, &stripes, &bins);
        assert!(err.is_err());
    }
}
