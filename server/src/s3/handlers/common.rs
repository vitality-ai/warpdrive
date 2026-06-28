// Shared utilities, constants, and types used across handler submodules.
use actix_web::{HttpRequest, HttpResponse, http::StatusCode};
use actix_web::body::{BodySize, MessageBody};

use bytes::Bytes;

use std::pin::Pin;
use std::task::{Context, Poll};

use crate::service::metadata_service::MetadataService;

pub(super) const S3_XMLNS: &str = "http://s3.amazonaws.com/doc/2006-03-01/";
pub(super) const S3_GET_STREAM_CHUNK: u64 = 8 * 1024 * 1024;

/// Empty body that reports a custom Content-Length for HEAD responses.
pub(super) struct HeadBody(pub(super) u64);
impl MessageBody for HeadBody {
    type Error = std::convert::Infallible;
    fn size(&self) -> BodySize { BodySize::Sized(self.0) }
    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Result<Bytes, Self::Error>>> {
        Poll::Ready(None)
    }
}

pub(super) enum RangeResult {
    None,
    Valid(u64, u64),
    Unsatisfiable,
}

pub(super) fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&'  => out.push_str("&amp;"),
            '<'  => out.push_str("&lt;"),
            '>'  => out.push_str("&gt;"),
            '"'  => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c    => out.push(c),
        }
    }
    out
}

pub(super) fn xml_unescape(s: &str) -> String {
    s.replace("&amp;", "&")
     .replace("&lt;", "<")
     .replace("&gt;", ">")
     .replace("&quot;", "\"")
     .replace("&apos;", "'")
}

/// Encode a stored metadata value as a HeaderValue for HTTP responses.
pub(super) fn metadata_value_header(v: &str) -> actix_web::http::header::HeaderValue {
    let bytes: Vec<u8> = v.chars()
        .map(|c| if (c as u32) <= 0xFF { c as u8 } else { b'?' })
        .collect();
    actix_web::http::header::HeaderValue::from_bytes(&bytes)
        .unwrap_or_else(|_| actix_web::http::header::HeaderValue::from_static(""))
}

/// S3 URL-encoding for listing responses.
pub(super) fn s3_url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' |
            b'-' | b'_' | b'.' | b'~' | b'/' => out.push(byte as char),
            b => { out.push('%'); out.push_str(&format!("{:02X}", b)); }
        }
    }
    out
}

/// Build an S3-spec XML error response.
pub(super) fn s3_error(status: StatusCode, code: &str, message: &str, resource: &str) -> HttpResponse {
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <Error>\n\
           <Code>{code}</Code>\n\
           <Message>{msg}</Message>\n\
           <Resource>{res}</Resource>\n\
           <RequestId>warpdrive</RequestId>\n\
         </Error>",
        code = xml_escape(code),
        msg  = xml_escape(message),
        res  = xml_escape(resource),
    );
    HttpResponse::build(status)
        .content_type("application/xml")
        .insert_header(("x-amz-request-id", "warpdrive"))
        .body(body)
}

/// Return 404 NoSuchBucket if the bucket is not registered for this user.
pub(super) fn require_bucket(db: &MetadataService, bucket: &str) -> Result<(), HttpResponse> {
    match db.bucket_exists(bucket) {
        Ok(true)  => Ok(()),
        Ok(false) => Err(s3_error(StatusCode::NOT_FOUND, "NoSuchBucket",
                                  "The specified bucket does not exist", bucket)),
        Err(_)    => Err(s3_error(StatusCode::INTERNAL_SERVER_ERROR, "InternalError",
                                  "Internal server error", bucket)),
    }
}

/// Split extent list into ≤ S3_GET_STREAM_CHUNK slices for streaming.
pub(super) fn stream_slices(chunks: &[(u64, u64)]) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    for &(base, total) in chunks {
        let mut off = base;
        let mut rem = total;
        while rem > 0 {
            let n = rem.min(S3_GET_STREAM_CHUNK);
            out.push((off, n));
            off += n;
            rem -= n;
        }
    }
    out
}

/// Parse `Range: bytes=X-Y`, `bytes=X-`, or `bytes=-N` (suffix).
pub(super) fn parse_range_header(req: &HttpRequest, total: u64) -> RangeResult {
    let hdr = match req.headers().get("range").and_then(|v| v.to_str().ok()) {
        Some(h) => h.to_string(),
        None => return RangeResult::None,
    };
    let bytes = match hdr.strip_prefix("bytes=") {
        Some(b) => b.to_string(),
        None => return RangeResult::Unsatisfiable,
    };
    if bytes.starts_with('-') {
        let n: u64 = match bytes[1..].parse().ok() {
            Some(n) => n,
            None => return RangeResult::Unsatisfiable,
        };
        if n == 0 || total == 0 {
            return RangeResult::Unsatisfiable;
        }
        let start = total.saturating_sub(n);
        return RangeResult::Valid(start, total - 1);
    }
    let (start_s, end_s) = match bytes.split_once('-') {
        Some(pair) => pair,
        None => return RangeResult::Unsatisfiable,
    };
    let start: u64 = match start_s.parse().ok() {
        Some(s) => s,
        None => return RangeResult::Unsatisfiable,
    };
    let end: u64 = if end_s.is_empty() {
        total.saturating_sub(1)
    } else {
        match end_s.parse::<u64>().ok() {
            Some(e) => e,
            None => return RangeResult::Unsatisfiable,
        }
    };
    if total == 0 || start >= total {
        return RangeResult::Unsatisfiable;
    }
    let end = end.min(total - 1);
    if start > end {
        return RangeResult::Unsatisfiable;
    }
    RangeResult::Valid(start, end)
}

/// Map a logical byte range onto storage extents.
pub(super) fn range_slices(chunks: &[(u64, u64)], range_start: u64, range_end: u64) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    let mut logical = 0u64;
    for &(storage_off, chunk_size) in chunks {
        let chunk_end = logical + chunk_size;
        if chunk_end <= range_start {
            logical = chunk_end;
            continue;
        }
        if logical >= range_end + 1 {
            break;
        }
        let read_start = range_start.max(logical);
        let read_end   = (range_end + 1).min(chunk_end);
        let skip       = read_start - logical;
        let read_size  = read_end - read_start;
        let mut off = storage_off + skip;
        let mut rem = read_size;
        while rem > 0 {
            let n = rem.min(S3_GET_STREAM_CHUNK);
            out.push((off, n));
            off += n;
            rem -= n;
        }
        logical = chunk_end;
    }
    out
}

/// Compute RFC 2616 date string from now.
pub(super) fn rfc2616_now() -> String {
    chrono::Utc::now().format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

/// Compute MD5 ETag (double-quoted) from accumulated bytes.
pub(super) fn md5_etag(data: &[u8]) -> String {
    format!("\"{}\"", hex::encode(md5::compute(data).0))
}

/// Strip surrounding double quotes from an ETag for comparison.
#[inline]
pub(super) fn normalize_etag(etag: &str) -> &str {
    etag.trim().trim_matches('"')
}

/// Return a 412 PreconditionFailed S3 error response.
#[inline]
pub(super) fn s3_precondition_failed(resource: &str) -> HttpResponse {
    s3_error(StatusCode::PRECONDITION_FAILED, "PreconditionFailed",
             "At least one of the pre-conditions you specified did not hold",
             resource)
}

/// Extract the text content of the first occurrence of `<tag>…</tag>` in `src`.
pub(super) fn extract_xml_tag(src: &str, tag: &str) -> Option<String> {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);
    let start = src.find(&open)? + open.len();
    let end = src[start..].find(&close)?;
    Some(src[start..start + end].to_string())
}

/// Extract text content of ALL occurrences of `<tag>…</tag>` in `src`.
pub(super) fn extract_all_xml_tags(src: &str, tag: &str) -> Vec<String> {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);
    let mut results = Vec::new();
    let mut remaining = src;
    while let Some(start) = remaining.find(&open) {
        let content_start = start + open.len();
        if let Some(end) = remaining[content_start..].find(&close) {
            results.push(remaining[content_start..content_start + end].to_string());
            remaining = &remaining[content_start + end + close.len()..];
        } else {
            break;
        }
    }
    results
}

/// Parse an HTTP date string into Unix seconds.
pub(super) fn parse_http_date(s: &str) -> Option<i64> {
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%a, %d %b %Y %H:%M:%S GMT") {
        return Some(dt.and_utc().timestamp());
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc2822(s) {
        return Some(dt.timestamp());
    }
    None
}

pub(super) fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Some(hex) = std::str::from_utf8(&bytes[i+1..i+3]).ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok())
            {
                out.push(hex);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub(super) fn req_query_map(req: &HttpRequest) -> std::collections::HashMap<String, String> {
    actix_web::web::Query::<std::collections::HashMap<String, String>>::from_query(req.query_string())
        .map(|q| q.into_inner())
        .unwrap_or_default()
}
