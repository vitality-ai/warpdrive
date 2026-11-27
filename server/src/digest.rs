//! Content digests every layer uses: object checksums (CRC32, CRC32C,
//! CRC64NVME, SHA1, SHA256), MD5 and the ETag built from it, and the
//! last-modified stamp. Below the front ends, so neither the object layer
//! nor the engine depends on S3's handlers. S3's header parsing stays in
//! `s3::handlers::checksum`.

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use sha2::{Sha256, Digest};
use sha1::Sha1;
use crc::{Crc, Algorithm};

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
        // SSE4.2 / ARMv8 CRC instructions where the CPU has them.
        ChecksumAlgorithm::Crc32c => B64.encode(crc32c::crc32c(data).to_be_bytes()),
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


/// MD5 of `data` (assembly on x86_64, see Cargo.toml).
pub fn md5_digest(data: &[u8]) -> [u8; 16] {
    use md5::Digest as _;
    md5::Md5::digest(data).into()
}

/// An object's ETag: the quoted hex MD5 of its bytes.
pub fn md5_etag(data: &[u8]) -> String {
    format!("\"{}\"", hex::encode(md5_digest(data)))
}

/// Now, as stored in an object's `last_modified` and emitted in S3 XML
/// bodies. ISO 8601 (`2026-07-01T12:00:00.000Z`).
pub fn last_modified_now() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S.000Z").to_string()
}

#[cfg(test)]
mod hardware_hash_tests {
    use super::*;

    /// The hardware CRC32C gives the standard CRC-32C (iSCSI) values, so
    /// checksums stored before and after the switch agree.
    #[test]
    fn crc32c_matches_the_standard_check_values() {
        assert_eq!(crc32c::crc32c(b"123456789"), 0xE306_9283);
        let table = Crc::<u32>::new(&crc::CRC_32_ISCSI);
        let data: Vec<u8> = (0..1_000_003u32).map(|i| (i * 31 % 251) as u8).collect();
        assert_eq!(crc32c::crc32c(&data), table.checksum(&data));
        assert_eq!(compute_checksum(&ChecksumAlgorithm::Crc32c, &data), B64.encode(table.checksum(&data).to_be_bytes()));
    }

    #[test]
    fn md5_digest_matches_the_standard_values() {
        use crate::digest::md5_digest;
        assert_eq!(hex::encode(md5_digest(b"")), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(hex::encode(md5_digest(b"The quick brown fox jumps over the lazy dog")), "9e107d9d372bb6826bd81d3542a419d6");
    }
}
