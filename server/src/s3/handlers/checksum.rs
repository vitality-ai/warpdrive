//! S3 checksum headers. The checksums themselves are in `crate::digest`.

use actix_web::HttpRequest;

pub use crate::digest::{compute_checksum, compute_composite_checksum, verify_checksum, ChecksumAlgorithm};

/// Parse checksum algorithm + value from request headers.
/// Returns (algo, client_provided_value) if both are present.
/// boto3/AWS SDK sends `x-amz-sdk-checksum-algorithm`; raw clients may send
/// `x-amz-checksum-algorithm`. We accept both.
pub fn parse_checksum_headers(req: &HttpRequest) -> Option<(ChecksumAlgorithm, String)> {
    let algo_str = req.headers().get("x-amz-sdk-checksum-algorithm")
        .or_else(|| req.headers().get("x-amz-checksum-algorithm"))
        .and_then(|v| v.to_str().ok())?;
    let algo = ChecksumAlgorithm::from_str(algo_str)?;
    let header_name = format!("x-amz-checksum-{}", algo.header_suffix());
    let value = req.headers().get(header_name.as_str())
        .and_then(|v| v.to_str().ok())?.to_string();
    Some((algo, value))
}

/// Extract only the algorithm name from request headers (without requiring a value header).
pub fn parse_checksum_algorithm(req: &HttpRequest) -> Option<ChecksumAlgorithm> {
    let algo_str = req.headers().get("x-amz-sdk-checksum-algorithm")
        .or_else(|| req.headers().get("x-amz-checksum-algorithm"))
        .and_then(|v| v.to_str().ok())?;
    ChecksumAlgorithm::from_str(algo_str)
}

