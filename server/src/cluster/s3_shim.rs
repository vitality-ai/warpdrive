//! Minimal S3-protocol surface over the distributed `/cluster/` API.
//!
//! Why this exists, separately from the already-real, already-tested S3 API
//! (`s3::handlers`, SigV4 auth, bucket operations): that API writes to
//! single-node local storage only, never through `cluster_put_object`'s
//! FAC/plain dispatch — pointing a real S3 client at it would prove
//! "talks to WarpDrive," not "reconfigurability helps." This shim is the
//! other direction: real-enough S3 protocol (GET/PUT/HEAD/LIST) in front of
//! the *same* dispatch DuckDB's Range-GET already proved, for clients like
//! Lance's `object_store::aws` that need genuine bucket/list semantics, not
//! just an arbitrary HTTP+Range URL the way DuckDB's `httpfs` accepts.
//!
//! Deliberately minimal, deliberately not a full S3 implementation: no
//! SigV4 verification (any `Authorization` header is accepted unchecked),
//! no multipart upload, no pagination on LIST. Scoped exactly to what
//! `object_store::aws` needs to open and read a dataset. User's own framing
//! for this: "minimal... then reevaluate the architecture... once we show
//! promising results" — this is that minimal pass, not a production S3
//! surface. GET/PUT object reuse `cluster_get_object`/`cluster_put_object`
//! directly (same Range support, same FAC dispatch) via a second route
//! prefix; only HEAD and LIST are new.

use actix_web::error::ErrorNotFound;
use actix_web::{web, Error, HttpResponse};

use super::coordinator::ClusterState;
use crate::s3::handlers::common::HeadBody;

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

pub async fn cluster_s3_head_object(
    path: web::Path<(String, String)>,
    state: web::Data<ClusterState>,
) -> Result<HttpResponse, Error> {
    let (bucket, key) = path.into_inner();

    let size = if let Some(record) = state.content_location_store.get(&bucket, &key) {
        record.original_len as u64
    } else if let Some(record) = state.location_store.get(&bucket, &key) {
        record.original_len as u64
    } else {
        return Err(ErrorNotFound("object not found"));
    };

    Ok(HttpResponse::Ok()
        .insert_header(("Accept-Ranges", "bytes"))
        .body(HeadBody(size)))
}

/// A minimal ListObjectsV2: no pagination (fine for a demo-scale dataset —
/// a Lance dataset here is tens to low hundreds of files, not millions),
/// merging both stores since a bucket's objects can be split across the
/// plain and content-dependent paths depending on what each PUT opted into.
pub async fn cluster_s3_list_objects(
    path: web::Path<String>,
    query: web::Query<std::collections::HashMap<String, String>>,
    state: web::Data<ClusterState>,
) -> Result<HttpResponse, Error> {
    let bucket = path.into_inner();
    let prefix = query.get("prefix").cloned().unwrap_or_default();

    let mut entries = state.location_store.list(&bucket, &prefix);
    entries.extend(state.content_location_store.list(&bucket, &prefix));
    entries.sort();
    entries.dedup();

    let contents: String = entries
        .iter()
        .map(|(key, size)| {
            format!(
                "<Contents><Key>{}</Key><Size>{size}</Size><LastModified>1970-01-01T00:00:00.000Z</LastModified>\
                 <ETag>&quot;0&quot;</ETag><StorageClass>STANDARD</StorageClass></Contents>",
                xml_escape(key)
            )
        })
        .collect();

    let body = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\n\
           <Name>{}</Name>\n\
           <Prefix>{}</Prefix>\n\
           <KeyCount>{}</KeyCount>\n\
           <MaxKeys>1000</MaxKeys>\n\
           <IsTruncated>false</IsTruncated>\n\
           {}\n\
         </ListBucketResult>",
        xml_escape(&bucket),
        xml_escape(&prefix),
        entries.len(),
        contents
    );

    Ok(HttpResponse::Ok().content_type("application/xml").body(body))
}
