//! WarpDrive-native endpoints (not part of the S3 API).

use actix_web::{web, HttpRequest, HttpResponse, Error};
use futures::stream::{self, StreamExt};
use log::{debug, warn};

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

    let objects = db.get_objects_by_slab(&bucket, hint.as_deref())?;
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

    // Build a streaming response: read one object at a time to avoid buffering
    // the entire slab (which can be many GB) into memory.
    let boundary = BOUNDARY;
    let object_stream = stream::iter(objects.into_iter())
        .flat_map(move |(key, extents)| {
            let storage = StorageService::new();
            let ctx = context.clone();
            let data = match storage.read_object(&ctx, &extents, crate::service::storage_service::StorageMode::S3) {
                Ok(d) => d,
                Err(e) => {
                    warn!("SlabBatchGet: read failed for key={}: {}", key, e);
                    return stream::iter(vec![]);
                }
            };
            // Content-Disposition's filename is a bare filename, not a path --
            // the client joins it onto its own local timeline directory
            // (timeline_path.join(&filename)). Sending the full S3 key here
            // produced a doubled, nonexistent nested path on the client side.
            let basename = key.rsplit('/').next().unwrap_or(&key);
            let part_header = format!(
                "--{boundary}\r\n\
                 Content-Disposition: attachment; filename=\"{basename}\"\r\n\
                 Content-Type: application/octet-stream\r\n\
                 Content-Length: {len}\r\n\
                 \r\n",
                boundary = boundary,
                basename = basename,
                len = data.len(),
            );
            let mut chunks: Vec<Result<web::Bytes, Error>> = Vec::with_capacity(3);
            chunks.push(Ok(web::Bytes::from(part_header.into_bytes())));
            chunks.push(Ok(web::Bytes::from(data)));
            chunks.push(Ok(web::Bytes::from_static(b"\r\n")));
            stream::iter(chunks)
        })
        .chain(stream::once(async move {
            Ok::<web::Bytes, Error>(web::Bytes::from(format!("--{}--\r\n", BOUNDARY)))
        }));

    let mut resp = HttpResponse::Ok();
    resp.content_type(format!("multipart/mixed; boundary={}", BOUNDARY));
    if let Some(ref h) = hint {
        resp.insert_header(("x-warpd-slab-hint", h.as_str()));
    }
    Ok(resp
        .insert_header(("x-warpd-slab-count", count.to_string().as_str()))
        .streaming(object_stream))
}
