//! Erasure coding behind a narrow trait (`ErasureCoder`), so the library or
//! shard-sizing strategy can be swapped later without touching coordinator.rs.
//! One object = one RS(k,m) stripe for now; multi-stripe for very large
//! objects is part of the deferred content-dependent work, not this phase.

use reed_solomon_erasure::galois_8::ReedSolomon;
use std::fmt;

#[derive(Debug)]
pub enum EcError {
    Config(String),
    Codec(String),
    TooFewShards,
}

impl fmt::Display for EcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EcError::Config(s) => write!(f, "erasure coding config error: {s}"),
            EcError::Codec(s) => write!(f, "erasure coding codec error: {s}"),
            EcError::TooFewShards => write!(f, "fewer than k shards available, cannot reconstruct"),
        }
    }
}
impl std::error::Error for EcError {}

/// `k` data shards followed by `m` parity shards for one object.
pub struct EncodedObject {
    pub shards: Vec<Vec<u8>>,
    pub original_len: usize,
}

pub trait ErasureCoder: Send + Sync {
    fn k(&self) -> usize;
    fn m(&self) -> usize;

    /// Pads `data` to a multiple of `k`, splits it into `k` equal data
    /// shards, and computes `m` parity shards.
    fn encode(&self, data: &[u8]) -> Result<EncodedObject, EcError>;

    /// Reconstructs the original `original_len` bytes from whichever of the
    /// `k+m` shards are present (`None` for missing ones). Needs at least `k`.
    fn decode(&self, shards: &[Option<Vec<u8>>], original_len: usize) -> Result<Vec<u8>, EcError>;

    /// Given exactly `k` pre-chunked, equal-size data shards — e.g. the `k`
    /// content-aware bins `fac_core::construct_stripes` produces, not a
    /// flat-buffer split — computes `m` parity shards and returns all
    /// `k+m`. Used by content-dependent placement, where bins are packed
    /// by content, not by splitting one flat buffer evenly.
    fn encode_shards(&self, data_shards: Vec<Vec<u8>>) -> Result<Vec<Vec<u8>>, EcError>;

    /// Reconstructs just the `k` data shards (not a flat original buffer)
    /// from whichever of `k+m` shards are present. The content-dependent
    /// counterpart to `decode`, which additionally knows how to trim back
    /// to a flat buffer's `original_len` — stripes don't have one single
    /// original length, so this stops at "the k data shards."
    fn decode_shards(&self, shards: &[Option<Vec<u8>>]) -> Result<Vec<Vec<u8>>, EcError>;
}

pub struct ReedSolomonCoder {
    k: usize,
    m: usize,
    /// `None` for `m == 0`: no parity, the data shards alone (a single node
    /// is RS(1,0)).
    rs: Option<ReedSolomon>,
}

impl ReedSolomonCoder {
    pub fn new(k: usize, m: usize) -> Result<Self, EcError> {
        if k == 0 {
            return Err(EcError::Config("k must be >= 1".into()));
        }
        let rs = if m == 0 { None } else { Some(ReedSolomon::new(k, m).map_err(|e| EcError::Config(format!("{e:?}")))?) };
        Ok(Self { k, m, rs })
    }

    /// Computes parity into `shards` (k data then m parity), if there is any.
    fn add_parity(&self, shards: &mut [Vec<u8>]) -> Result<(), EcError> {
        match &self.rs {
            Some(rs) => rs.encode(shards).map_err(|e| EcError::Codec(format!("{e:?}"))),
            None => Ok(()),
        }
    }

    /// Fills in missing data shards from parity. Without parity every data
    /// shard has to be present.
    fn reconstruct(&self, shards: &mut [Option<Vec<u8>>]) -> Result<(), EcError> {
        match &self.rs {
            Some(rs) => rs.reconstruct_data(shards).map_err(|e| EcError::Codec(format!("{e:?}"))),
            None if shards.iter().take(self.k).all(|s| s.is_some()) => Ok(()),
            None => Err(EcError::TooFewShards),
        }
    }
}

impl ErasureCoder for ReedSolomonCoder {
    fn k(&self) -> usize { self.k }
    fn m(&self) -> usize { self.m }

    fn encode(&self, data: &[u8]) -> Result<EncodedObject, EcError> {
        let original_len = data.len();
        let shard_len = ((original_len + self.k - 1) / self.k).max(1);

        let mut shards: Vec<Vec<u8>> = Vec::with_capacity(self.k + self.m);
        for i in 0..self.k {
            let start = (i * shard_len).min(original_len);
            let end = (start + shard_len).min(original_len);
            let mut shard = vec![0u8; shard_len];
            shard[..end - start].copy_from_slice(&data[start..end]);
            shards.push(shard);
        }
        for _ in 0..self.m {
            shards.push(vec![0u8; shard_len]);
        }

        self.add_parity(&mut shards)?;

        Ok(EncodedObject { shards, original_len })
    }

    fn decode(&self, shards: &[Option<Vec<u8>>], original_len: usize) -> Result<Vec<u8>, EcError> {
        let present = shards.iter().filter(|s| s.is_some()).count();
        if present < self.k {
            return Err(EcError::TooFewShards);
        }

        let mut shards: Vec<Option<Vec<u8>>> = shards.to_vec();
        self.reconstruct(&mut shards)?;

        let mut data = Vec::with_capacity(original_len);
        for shard in shards.iter().take(self.k) {
            let shard = shard.as_ref().expect("reconstruct_data fills in all data shards");
            data.extend_from_slice(shard);
        }
        data.truncate(original_len);
        Ok(data)
    }

    fn encode_shards(&self, mut data_shards: Vec<Vec<u8>>) -> Result<Vec<Vec<u8>>, EcError> {
        if data_shards.len() != self.k {
            return Err(EcError::Config(format!(
                "expected {} data shards, got {}",
                self.k,
                data_shards.len()
            )));
        }
        let shard_len = data_shards[0].len();
        if data_shards.iter().any(|s| s.len() != shard_len) {
            return Err(EcError::Config("all data shards must be the same size".into()));
        }
        for _ in 0..self.m {
            data_shards.push(vec![0u8; shard_len]);
        }
        self.add_parity(&mut data_shards)?;
        Ok(data_shards)
    }

    fn decode_shards(&self, shards: &[Option<Vec<u8>>]) -> Result<Vec<Vec<u8>>, EcError> {
        let present = shards.iter().filter(|s| s.is_some()).count();
        if present < self.k {
            return Err(EcError::TooFewShards);
        }
        let mut shards: Vec<Option<Vec<u8>>> = shards.to_vec();
        self.reconstruct(&mut shards)?;
        Ok(shards
            .into_iter()
            .take(self.k)
            .map(|s| s.expect("reconstruct_data fills in all data shards"))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_round_trip_full() {
        let coder = ReedSolomonCoder::new(4, 2).unwrap();
        let data = b"the quick brown fox jumps over the lazy dog, a long enough test payload".to_vec();
        let encoded = coder.encode(&data).unwrap();
        assert_eq!(encoded.shards.len(), 6);

        let shards: Vec<Option<Vec<u8>>> = encoded.shards.iter().cloned().map(Some).collect();
        let decoded = coder.decode(&shards, encoded.original_len).unwrap();
        assert_eq!(decoded, data);
    }

    #[test]
    fn encode_decode_round_trip_missing_shard() {
        let coder = ReedSolomonCoder::new(4, 2).unwrap();
        let data = b"a payload that needs to survive the loss of exactly one shard out of six".to_vec();
        let encoded = coder.encode(&data).unwrap();

        let mut shards: Vec<Option<Vec<u8>>> = encoded.shards.iter().cloned().map(Some).collect();
        shards[1] = None; // drop one data shard; parity must cover it

        let decoded = coder.decode(&shards, encoded.original_len).unwrap();
        assert_eq!(decoded, data);
    }

    #[test]
    fn decode_fails_with_too_few_shards() {
        let coder = ReedSolomonCoder::new(4, 2).unwrap();
        let data = b"short".to_vec();
        let encoded = coder.encode(&data).unwrap();

        let mut shards: Vec<Option<Vec<u8>>> = encoded.shards.iter().cloned().map(Some).collect();
        shards[0] = None;
        shards[1] = None;
        shards[2] = None; // only 3 of 6 shards left, k=4 needed

        assert!(matches!(
            coder.decode(&shards, encoded.original_len),
            Err(EcError::TooFewShards)
        ));
    }

    #[test]
    fn encode_shards_round_trips_pre_chunked_bins() {
        let coder = ReedSolomonCoder::new(3, 2).unwrap();
        // Pre-chunked, equal-size "bins" like construct_stripes would hand
        // over — not a flat buffer split evenly.
        let bins = vec![
            b"bin zero content".to_vec(),
            b"bin one contents".to_vec(),
            b"bin two content.".to_vec(),
        ];
        let all_shards = coder.encode_shards(bins.clone()).unwrap();
        assert_eq!(all_shards.len(), 5); // k=3 + m=2

        let mut with_gap: Vec<Option<Vec<u8>>> = all_shards.iter().cloned().map(Some).collect();
        with_gap[1] = None; // lose one data bin; parity must cover it

        let recovered = coder.decode_shards(&with_gap).unwrap();
        assert_eq!(recovered, bins);
    }

    #[test]
    fn encode_shards_rejects_wrong_shard_count() {
        let coder = ReedSolomonCoder::new(3, 2).unwrap();
        let bins = vec![b"only one bin".to_vec()];
        assert!(matches!(coder.encode_shards(bins), Err(EcError::Config(_))));
    }

    #[test]
    fn rs_1_0_is_the_data_itself_and_needs_it_all() {
        let coder = ReedSolomonCoder::new(1, 0).unwrap();
        let enc = coder.encode(b"hello world").unwrap();
        assert_eq!(enc.shards, vec![b"hello world".to_vec()], "one shard, no parity");
        assert_eq!(coder.decode(&[Some(enc.shards[0].clone())], enc.original_len).unwrap(), b"hello world");
        assert!(matches!(coder.decode(&[None], enc.original_len), Err(EcError::TooFewShards)));
        let k2 = ReedSolomonCoder::new(2, 0).unwrap();
        let enc = k2.encode(b"abcde").unwrap();
        assert_eq!(enc.shards.len(), 2);
        assert_eq!(k2.decode(&enc.shards.iter().cloned().map(Some).collect::<Vec<_>>(), 5).unwrap(), b"abcde");
        assert!(ReedSolomonCoder::new(0, 1).is_err());
    }
}
