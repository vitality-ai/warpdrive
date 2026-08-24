//! S3 Checksum support utilities

use actix_web::HttpRequest;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use sha2::{Sha256, Digest};
use sha1::Sha1;
use crc::{Crc, CRC_32_ISCSI, Algorithm};
use lazy_static::lazy_static;

const CRC_64_NVME_ALGO: Algorithm<u64> = Algorithm {
    width: 64,
    poly: 0xad93d23594c93659u64,
    init: 0xffffffffffffffffu64,
    refin: true,
    refout: true,
    xorout: 0xffffffffffffffffu64,
    check: 0xae8b14860a799888u64,
    residue: 0,
};

#[derive(Debug, Clone, PartialEq)]
pub enum ChecksumAlgorithm {
    Sha256,
    Crc32,
    Crc32c,
    Sha1,
    Crc64Nvme,
}

impl ChecksumAlgorithm {
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_uppercase().as_str() {
            "SHA256" => Some(Self::Sha256),
            "CRC32" => Some(Self::Crc32),
            "CRC32C" => Some(Self::Crc32c),
            "SHA1" => Some(Self::Sha1),
            "CRC64NVME" => Some(Self::Crc64Nvme),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Sha256 => "SHA256",
            Self::Crc32 => "CRC32",
            Self::Crc32c => "CRC32C",
            Self::Sha1 => "SHA1",
            Self::Crc64Nvme => "CRC64NVME",
        }
    }

    /// Header suffix for `x-amz-checksum-{suffix}`.
    pub fn header_suffix(&self) -> &'static str {
        match self {
            Self::Sha256 => "sha256",
            Self::Crc32 => "crc32",
            Self::Crc32c => "crc32c",
            Self::Sha1 => "sha1",
            Self::Crc64Nvme => "crc64nvme",
        }
    }

    /// Default ChecksumType for multipart uploads when not explicitly specified.
    /// COMPOSITE for SHA256/SHA1 (hash-based), FULL_OBJECT for CRC types.
    pub fn default_type(&self) -> &'static str {
        match self {
            Self::Sha256 | Self::Sha1 => "COMPOSITE",
            _ => "FULL_OBJECT",
        }
    }

    /// XML element name used in GetObjectAttributes Checksum element.
    pub fn response_key(&self) -> &'static str {
        match self {
            Self::Sha256 => "ChecksumSHA256",
            Self::Crc32 => "ChecksumCRC32",
            Self::Crc32c => "ChecksumCRC32C",
            Self::Sha1 => "ChecksumSHA1",
            Self::Crc64Nvme => "ChecksumCRC64NVME",
        }
    }
}

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

/// Compute checksum of data, return base64-encoded string.
pub fn compute_checksum(algo: &ChecksumAlgorithm, data: &[u8]) -> String {
    match algo {
        ChecksumAlgorithm::Sha256 => {
            let hash = Sha256::digest(data);
            B64.encode(hash)
        }
        ChecksumAlgorithm::Sha1 => {
            let hash = Sha1::digest(data);
            B64.encode(hash)
        }
        ChecksumAlgorithm::Crc32 => {
            let crc = crc32fast::hash(data);
            B64.encode(crc.to_be_bytes())
        }
        ChecksumAlgorithm::Crc32c => {
            let crc = Crc::<u32>::new(&CRC_32_ISCSI);
            B64.encode(crc.checksum(data).to_be_bytes())
        }
        ChecksumAlgorithm::Crc64Nvme => {
            let crc = Crc::<u64>::new(&CRC_64_NVME_ALGO);
            B64.encode(crc.checksum(data).to_be_bytes())
        }
    }
}

/// Verify client-provided base64 checksum against computed. Returns true if match.
pub fn verify_checksum(algo: &ChecksumAlgorithm, data: &[u8], expected_b64: &str) -> bool {
    compute_checksum(algo, data) == expected_b64
}

/// Compute COMPOSITE checksum (for SHA256/SHA1):
/// hash(concat(base64_decode(part1_cksum), base64_decode(part2_cksum), ...))
/// Returns "base64(hash(...))-N"
pub fn compute_composite_checksum(algo: &ChecksumAlgorithm, part_checksums: &[String]) -> String {
    let mut combined = Vec::new();
    for cksum in part_checksums {
        if let Ok(decoded) = B64.decode(cksum.trim()) {
            combined.extend_from_slice(&decoded);
        }
    }
    let hash_b64 = compute_checksum(algo, &combined);
    format!("{}-{}", hash_b64, part_checksums.len())
}

// ---------------------------------------------------------------------------
// Incremental (streaming) checksum -- used on the PUT path so objects above
// the in-memory buffering threshold never need their full body materialized
// just to compute a checksum. Below that threshold, the plain whole-buffer
// functions above are simpler and are used instead.
// ---------------------------------------------------------------------------

lazy_static! {
    static ref CRC32C_TABLE: Crc<u32> = Crc::<u32>::new(&CRC_32_ISCSI);
    static ref CRC64NVME_TABLE: Crc<u64> = Crc::<u64>::new(&CRC_64_NVME_ALGO);
}

pub enum ChecksumHasher {
    Sha256(Sha256),
    Sha1(Sha1),
    Crc32(crc32fast::Hasher),
    Crc32c(crc::Digest<'static, u32>),
    Crc64Nvme(crc::Digest<'static, u64>),
}

impl ChecksumHasher {
    pub fn new(algo: &ChecksumAlgorithm) -> Self {
        match algo {
            ChecksumAlgorithm::Sha256 => Self::Sha256(Sha256::new()),
            ChecksumAlgorithm::Sha1 => Self::Sha1(Sha1::new()),
            ChecksumAlgorithm::Crc32 => Self::Crc32(crc32fast::Hasher::new()),
            ChecksumAlgorithm::Crc32c => Self::Crc32c(CRC32C_TABLE.digest()),
            ChecksumAlgorithm::Crc64Nvme => Self::Crc64Nvme(CRC64NVME_TABLE.digest()),
        }
    }

    pub fn update(&mut self, data: &[u8]) {
        match self {
            Self::Sha256(h) => Digest::update(h, data),
            Self::Sha1(h) => Digest::update(h, data),
            Self::Crc32(h) => h.update(data),
            Self::Crc32c(h) => h.update(data),
            Self::Crc64Nvme(h) => h.update(data),
        }
    }

    pub fn finalize_b64(self) -> String {
        match self {
            Self::Sha256(h) => B64.encode(h.finalize()),
            Self::Sha1(h) => B64.encode(h.finalize()),
            Self::Crc32(h) => B64.encode(h.finalize().to_be_bytes()),
            Self::Crc32c(h) => B64.encode(h.finalize().to_be_bytes()),
            Self::Crc64Nvme(h) => B64.encode(h.finalize().to_be_bytes()),
        }
    }
}

