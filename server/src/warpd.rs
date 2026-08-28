//! WarpDrive-native endpoints (not part of the S3 API).

use actix_web::{web, HttpRequest, HttpResponse, Error};
use log::{debug, info, warn};
use std::time::Instant;

use crate::s3::auth::authenticate_s3_request;
use crate::service::metadata_service::MetadataService;
use crate::service::storage_service::StorageService;
use crate::service::user_context::UserContext;

// ---------------------------------------------------------------------------
// GET /_warpd/slab/{bucket}?hint={hint}
//
// Returns all live objects stored under the given slab hint as a
// multipart/mixed response.  Each part carries:
//   Content-Disposition: attachment; filename="{key}"
//   Content-Type: application/octet-stream
//   Content-Length: {size}
//
// Neon's pageserver can call this once per checkpoint epoch and receive all
// k delta layers in a single round trip, reducing R_GET = T·λ·ρ·k to
// R_GET = T·λ·ρ·1.
// ---------------------------------------------------------------------------

const BOUNDARY: &str = "warpd_slab_boundary_v1";

pub async fn warpd_slab_batch_get(
    path: web::Path<String>,
    query: web::Query<std::collections::HashMap<String, String>>,
    req: HttpRequest,
) -> Result<HttpResponse, Error> {
    let bucket = path.into_inner();
    // hint is optional: when omitted, all slab objects in the bucket are returned.
    let hint: Option<String> = query
        .get("hint")
        .filter(|h| !h.is_empty())
        .cloned();

    let auth_result = authenticate_s3_request(&req).await?;
    let db = MetadataService::new(&auth_result.user_id)?;

    if !db.bucket_exists(&bucket)? {
        return Ok(HttpResponse::NotFound().body(format!("bucket {} not found", bucket)));
    }

    let t_start = Instant::now();
    let objects = db.get_objects_by_slab(&bucket, hint.as_deref())?;
    let t_query = t_start.elapsed();
    if objects.is_empty() {
        let mut resp = HttpResponse::NoContent();
        if let Some(ref h) = hint {
            resp.insert_header(("x-warpd-slab-hint", h.as_str()));
        }
        return Ok(resp.insert_header(("x-warpd-slab-count", "0")).finish());
    }

    debug!(
        "SlabBatchGet: bucket={} hint={:?} objects={}",
        bucket, hint, objects.len()
    );

    let context = UserContext::with_bucket(auth_result.user_id.clone(), bucket.clone());
    let count = objects.len();
    let boundary = BOUNDARY;

    // EXPERIMENT (hardcoded, not adaptive yet): build the whole multipart
    // body in one buffer instead of streaming three tiny chunks per object
    // (header/data/trailer -- 3*k stream polls, each individually subject to
    // HTTP chunked-framing overhead). This trades back to buffering the full
    // batch in memory, which the original streaming design was written to
    // avoid for very large slabs -- fine for this test (bounded batch sizes,
    // plenty of RAM), not a permanent fix. TODO: make this adaptive -- buffer
    // and send in one write when the batch comfortably fits in memory and
    // there's bandwidth to spare, fall back to the streaming path above for
    // batches large enough that buffering risks OOM.
    // Pre-reserve the buffer's full capacity up front. Without this, growing
    // a multi-GB Vec via repeated extend_from_slice triggers reallocation at
    // each capacity doubling, and each reallocation copies the *entire*
    // buffer built so far -- for a ~2GB body that's multiple full-buffer
    // memcpys, which dominated wall time far more than the actual disk reads
    // (confirmed: this alone was slower than the streaming version it
    // replaced, until this fix).
    let estimated_size: u64 = objects
        .iter()
        .map(|(_, extents)| extents.iter().map(|(_, size)| *size).sum::<u64>() + 256)
        .sum();
    let mut body: Vec<u8> = Vec::with_capacity(estimated_size as usize);
    let mut total_bytes: u64 = 0;
    let mut read_micros: u64 = 0;
    let mut copy_micros: u64 = 0;
    let t_read_start = Instant::now();
    for (key, extents) in objects.into_iter() {
        let t_read = Instant::now();
        let data = match StorageService::new().read_object(&context, &extents, crate::service::storage_service::StorageMode::S3) {
            Ok(d) => d,
            Err(e) => {
                warn!("SlabBatchGet: read failed for key={}: {}", key, e);
                continue;
            }
        };
        read_micros += t_read.elapsed().as_micros() as u64;
        total_bytes += data.len() as u64;
        // Content-Disposition's filename is a bare filename, not a path --
        // the client joins it onto its own local timeline directory
        // (timeline_path.join(&filename)). Sending the full S3 key here
        // produced a doubled, nonexistent nested path on the client side.
        let basename = key.rsplit('/').next().unwrap_or(&key);
        let t_copy = Instant::now();
        body.extend_from_slice(
            format!(
                "--{boundary}\r\n\
                 Content-Disposition: attachment; filename=\"{basename}\"\r\n\
                 Content-Type: application/octet-stream\r\n\
                 Content-Length: {len}\r\n\
                 \r\n",
                boundary = boundary,
                basename = basename,
                len = data.len(),
            )
            .as_bytes(),
        );
        body.extend_from_slice(&data);
        body.extend_from_slice(b"\r\n");
        copy_micros += t_copy.elapsed().as_micros() as u64;
    }
    body.extend_from_slice(format!("--{}--\r\n", boundary).as_bytes());
    let t_build = t_read_start.elapsed();

    info!(
        "SlabBatchGet timing (buffered): bucket={} hint={:?} objects={} sqlite_query={:?} total_bytes_read={} read_time={:?} copy_time={:?} read_and_build_time={:?} handler_wall={:?}",
        bucket, hint, count, t_query, total_bytes,
        std::time::Duration::from_micros(read_micros),
        std::time::Duration::from_micros(copy_micros),
        t_build, t_start.elapsed(),
    );

    let mut resp = HttpResponse::Ok();
    resp.content_type(format!("multipart/mixed; boundary={}", BOUNDARY));
    if let Some(ref h) = hint {
        resp.insert_header(("x-warpd-slab-hint", h.as_str()));
    }
    Ok(resp
        .insert_header(("x-warpd-slab-count", count.to_string().as_str()))
        .body(body))
}
