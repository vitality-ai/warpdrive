//! Inter-node shard transport, behind a trait so HTTP and a future
//! gRPC/FlatBuffers implementation are interchangeable — see the transport
//! decision point in docs/Distributed-Engine-Plan.md.
//!
//! The HTTP implementation calls WarpDrive's own existing native API
//! (`/put/{key}`, `/get/{key}`) machine-to-machine: no SigV4 auth (the
//! native API only reads `User`/`Bucket` headers, unlike the S3 API), no
//! bucket pre-registration, and the same FlatBuffers payload format the
//! native API already uses — reusing tested code rather than inventing a
//! protocol. Shards live under a reserved internal user namespace so they
//! never collide with a real tenant's own objects.

use actix_web::error::{ErrorBadGateway, ErrorInternalServerError};
use actix_web::Error;
use async_trait::async_trait;
use flatbuffers::{root, FlatBufferBuilder};
use std::time::Duration;

use crate::util::flatbuffer_store_generated::store::{
    FileData, FileDataArgs, FileDataList, FileDataListArgs,
};

use super::shard_storage::{shard_key, CLUSTER_SHARD_USER};

fn encode_single_file_flatbuffer(data: &[u8]) -> Vec<u8> {
    let mut builder = FlatBufferBuilder::new();
    let data_vector = builder.create_vector(data);
    let file_data = FileData::create(&mut builder, &FileDataArgs { data: Some(data_vector) });
    let files = builder.create_vector(&[file_data]);
    let file_data_list = FileDataList::create(&mut builder, &FileDataListArgs { files: Some(files) });
    builder.finish(file_data_list, None);
    builder.finished_data().to_vec()
}

fn decode_single_file_flatbuffer(body: &[u8]) -> Result<Vec<u8>, Error> {
    let file_data_list = root::<FileDataList>(body)
        .map_err(|e| ErrorBadGateway(format!("peer returned malformed FlatBuffers data: {e:?}")))?;
    let files = file_data_list
        .files()
        .ok_or_else(|| ErrorBadGateway("peer response had no files"))?;
    let first = files.get(0);
    let data = first
        .data()
        .ok_or_else(|| ErrorBadGateway("peer response file had no data"))?;
    Ok(data.bytes().to_vec())
}

#[async_trait]
pub trait PeerClient: Send + Sync {
    async fn put_shard(
        &self,
        peer: &str,
        bucket: &str,
        key: &str,
        shard_idx: usize,
        data: Vec<u8>,
    ) -> Result<(), Error>;

    async fn get_shard(
        &self,
        peer: &str,
        bucket: &str,
        key: &str,
        shard_idx: usize,
    ) -> Result<Vec<u8>, Error>;
}

pub struct HttpPeerClient {
    client: reqwest::Client,
}

impl HttpPeerClient {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("failed to build reqwest client for HttpPeerClient");
        Self { client }
    }
}

impl Default for HttpPeerClient {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl PeerClient for HttpPeerClient {
    async fn put_shard(
        &self,
        peer: &str,
        bucket: &str,
        key: &str,
        shard_idx: usize,
        data: Vec<u8>,
    ) -> Result<(), Error> {
        let sk = shard_key(bucket, key, shard_idx);
        let url = format!("{}/put/{}", peer.trim_end_matches('/'), sk);
        let body = encode_single_file_flatbuffer(&data);

        let resp = self
            .client
            .post(&url)
            .header("User", CLUSTER_SHARD_USER)
            .header("Bucket", bucket)
            .body(body)
            .send()
            .await
            .map_err(|e| ErrorBadGateway(format!("put_shard to {peer} failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(ErrorBadGateway(format!(
                "put_shard to {peer} returned {status}: {text}"
            )));
        }
        Ok(())
    }

    async fn get_shard(
        &self,
        peer: &str,
        bucket: &str,
        key: &str,
        shard_idx: usize,
    ) -> Result<Vec<u8>, Error> {
        let sk = shard_key(bucket, key, shard_idx);
        let url = format!("{}/get/{}", peer.trim_end_matches('/'), sk);

        let resp = self
            .client
            .get(&url)
            .header("User", CLUSTER_SHARD_USER)
            .header("Bucket", bucket)
            .send()
            .await
            .map_err(|e| ErrorBadGateway(format!("get_shard from {peer} failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(ErrorBadGateway(format!(
                "get_shard from {peer} returned {status}: {text}"
            )));
        }
        let body = resp
            .bytes()
            .await
            .map_err(|e| ErrorInternalServerError(format!("reading peer response failed: {e}")))?;
        decode_single_file_flatbuffer(&body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flatbuffer_round_trip() {
        let data = b"a shard's worth of bytes".to_vec();
        let encoded = encode_single_file_flatbuffer(&data);
        let decoded = decode_single_file_flatbuffer(&encoded).unwrap();
        assert_eq!(decoded, data);
    }

    #[test]
    fn shard_key_is_stable_and_distinct_per_index() {
        let a = shard_key("mybucket", "mykey", 0);
        let b = shard_key("mybucket", "mykey", 1);
        assert_ne!(a, b);
        assert_eq!(a, shard_key("mybucket", "mykey", 0));
    }
}
