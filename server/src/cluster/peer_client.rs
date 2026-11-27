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
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use std::time::Duration;

use crate::util::flatbuffer_store_generated::store::{
    FileData, FileDataArgs, FileDataList, FileDataListArgs,
};

use super::shard_storage::shard_key;

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
        user_id: &str,
        bucket: &str,
        key: &str,
        shard_idx: usize,
        data: Vec<u8>,
    ) -> Result<(), Error>;

    async fn get_shard(
        &self,
        peer: &str,
        user_id: &str,
        bucket: &str,
        key: &str,
        shard_idx: usize,
    ) -> Result<Vec<u8>, Error>;

    /// The largest single shard this transport can actually carry, if it has
    /// a fixed cap — `None` means "no known cap" (HTTP and raw-TCP have no
    /// built-in per-message limit here). Lets a caller reject an oversized
    /// PUT upfront with a clear error (#159) instead of discovering the
    /// limit deep inside a peer RPC, where today it surfaces only as
    /// "write quorum not met: 0/N" with no indication why every peer
    /// refused.
    fn max_shard_size(&self) -> Option<usize> {
        None
    }
}

/// Where this node keeps its own shards, as its shard server stores them.
pub trait LocalShards: Send + Sync {
    fn store(&self, user_id: &str, bucket: &str, key: &str, shard_idx: usize, data: &[u8]) -> Result<(), String>;
    fn load(&self, user_id: &str, bucket: &str, key: &str, shard_idx: usize) -> Result<Vec<u8>, String>;
}

/// This node's shard files (`shard_storage`).
pub struct DiskShards;

impl LocalShards for DiskShards {
    fn store(&self, user_id: &str, bucket: &str, key: &str, shard_idx: usize, data: &[u8]) -> Result<(), String> {
        super::shard_storage::store_shard(user_id, bucket, key, shard_idx, data).map_err(|e| e.to_string())
    }
    fn load(&self, user_id: &str, bucket: &str, key: &str, shard_idx: usize) -> Result<Vec<u8>, String> {
        super::shard_storage::load_shard(user_id, bucket, key, shard_idx).map_err(|e| e.to_string())
    }
}

/// Wraps a transport so this node's own shards stay in process: a shard
/// for this node (`me`) goes straight to `local`, as its shard server
/// would store it on receiving it, instead of over loopback to that
/// server. Shards for other nodes go over `inner`. The metadata record
/// already works this way (`coordinator::send_to_peers`).
pub struct LocalFirst {
    inner: std::sync::Arc<dyn PeerClient>,
    local: std::sync::Arc<dyn LocalShards>,
    me: Option<String>,
}

impl LocalFirst {
    /// `me` is this node's peer address, if it has one.
    pub fn new(inner: std::sync::Arc<dyn PeerClient>, local: std::sync::Arc<dyn LocalShards>, me: Option<&str>) -> Self {
        Self { inner, local, me: me.map(|m| m.trim().trim_end_matches('/').to_string()) }
    }

    fn is_me(&self, peer: &str) -> bool {
        self.me.as_deref().is_some_and(|me| peer.trim().trim_end_matches('/') == me)
    }
}

#[async_trait]
impl PeerClient for LocalFirst {
    async fn put_shard(&self, peer: &str, user_id: &str, bucket: &str, key: &str, shard_idx: usize, data: Vec<u8>) -> Result<(), Error> {
        if !self.is_me(peer) {
            return self.inner.put_shard(peer, user_id, bucket, key, shard_idx, data).await;
        }
        let (local, u, b, k) = (std::sync::Arc::clone(&self.local), user_id.to_string(), bucket.to_string(), key.to_string());
        tokio::task::spawn_blocking(move || local.store(&u, &b, &k, shard_idx, &data))
            .await
            .map_err(|e| ErrorBadGateway(format!("local shard store task failed: {e}")))?
            .map_err(|e| ErrorBadGateway(format!("put_shard to {peer} (this node) failed: {e}")))
    }

    async fn get_shard(&self, peer: &str, user_id: &str, bucket: &str, key: &str, shard_idx: usize) -> Result<Vec<u8>, Error> {
        if !self.is_me(peer) {
            return self.inner.get_shard(peer, user_id, bucket, key, shard_idx).await;
        }
        let (local, u, b, k) = (std::sync::Arc::clone(&self.local), user_id.to_string(), bucket.to_string(), key.to_string());
        tokio::task::spawn_blocking(move || local.load(&u, &b, &k, shard_idx))
            .await
            .map_err(|e| ErrorBadGateway(format!("local shard load task failed: {e}")))?
            .map_err(|e| ErrorBadGateway(format!("get_shard from {peer} (this node) failed: {e}")))
    }

    fn max_shard_size(&self) -> Option<usize> {
        self.inner.max_shard_size()
    }
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
        user_id: &str,
        bucket: &str,
        key: &str,
        shard_idx: usize,
        data: Vec<u8>,
    ) -> Result<(), Error> {
        let sk = shard_key(user_id, bucket, key, shard_idx);
        // #149: actix-web fully percent-*decodes* path parameters, so an
        // unencoded `sk` containing `?`, `#`, or `%` could be
        // reinterpreted as a query string, a fragment, or a different
        // literal byte sequence by the time it reached the receiving
        // peer's handler -- a URL built this way can silently misdirect
        // to a different shard key than the one actually intended.
        // `NON_ALPHANUMERIC` rather than hand-picking a "path-segment
        // safe" set: `sk` is an internal, opaque identifier never meant
        // to be read, so there's nothing to gain from a smaller, more
        // readable charset and real risk in getting one wrong the same
        // way the original bug did.
        let encoded_sk = utf8_percent_encode(&sk, NON_ALPHANUMERIC).to_string();
        let url = format!("{}/put/{}", peer.trim_end_matches('/'), encoded_sk);
        let body = encode_single_file_flatbuffer(&data);

        // Tenant-isolation fix: this used to hardcode a reserved
        // "__cluster__" user namespace here; the real `user_id` now
        // reaches the receiving peer's native `/put/{key}` handler the
        // same way a direct single-node client request would (that
        // handler's own `Storage`/`MetadataService` layer is already
        // keyed by this header's value, see `api::put`), so shard bytes
        // land in the correct per-tenant storage directory.
        let resp = self
            .client
            .post(&url)
            .header("User", user_id)
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
        user_id: &str,
        bucket: &str,
        key: &str,
        shard_idx: usize,
    ) -> Result<Vec<u8>, Error> {
        let sk = shard_key(user_id, bucket, key, shard_idx);
        // #149: same reasoning as `put_shard` above.
        let encoded_sk = utf8_percent_encode(&sk, NON_ALPHANUMERIC).to_string();
        let url = format!("{}/get/{}", peer.trim_end_matches('/'), encoded_sk);

        let resp = self
            .client
            .get(&url)
            .header("User", user_id)
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
    use std::sync::Arc;

    /// Records where each shard went.
    #[derive(Default)]
    struct Seen(parking_lot::Mutex<Vec<String>>);

    #[async_trait]
    impl PeerClient for Seen {
        async fn put_shard(&self, peer: &str, _u: &str, _b: &str, key: &str, _i: usize, _d: Vec<u8>) -> Result<(), Error> {
            self.0.lock().push(format!("remote {peer} {key}"));
            Ok(())
        }
        async fn get_shard(&self, peer: &str, _u: &str, _b: &str, key: &str, _i: usize) -> Result<Vec<u8>, Error> {
            self.0.lock().push(format!("remote {peer} {key}"));
            Ok(b"remote".to_vec())
        }
    }

    #[derive(Default)]
    struct Mem(parking_lot::Mutex<std::collections::HashMap<String, Vec<u8>>>);

    impl LocalShards for Mem {
        fn store(&self, _u: &str, _b: &str, key: &str, idx: usize, data: &[u8]) -> Result<(), String> {
            self.0.lock().insert(format!("{key}#{idx}"), data.to_vec());
            Ok(())
        }
        fn load(&self, _u: &str, _b: &str, key: &str, idx: usize) -> Result<Vec<u8>, String> {
            self.0.lock().get(&format!("{key}#{idx}")).cloned().ok_or_else(|| "no such shard".into())
        }
    }

    #[tokio::test]
    async fn local_first_keeps_this_nodes_shards_in_process() {
        let (remote, local) = (Arc::new(Seen::default()), Arc::new(Mem::default()));
        let c = LocalFirst::new(remote.clone(), local.clone(), Some("http://me:1/"));
        c.put_shard("http://me:1", "u", "b", "k", 0, b"mine".to_vec()).await.unwrap();
        c.put_shard("http://other:1", "u", "b", "k", 1, b"theirs".to_vec()).await.unwrap();
        assert_eq!(c.get_shard("http://me:1/", "u", "b", "k", 0).await.unwrap(), b"mine");
        assert_eq!(local.0.lock().len(), 1, "only this node's shard is stored locally");
        assert_eq!(*remote.0.lock(), vec!["remote http://other:1 k".to_string()]);
        let anon = LocalFirst::new(remote.clone(), local, None);
        assert_eq!(anon.get_shard("http://me:1", "u", "b", "k", 0).await.unwrap(), b"remote", "no address: every shard is remote");
    }

    #[test]
    fn flatbuffer_round_trip() {
        let data = b"a shard's worth of bytes".to_vec();
        let encoded = encode_single_file_flatbuffer(&data);
        let decoded = decode_single_file_flatbuffer(&encoded).unwrap();
        assert_eq!(decoded, data);
    }

    #[test]
    fn shard_key_is_stable_and_distinct_per_index() {
        let a = shard_key("u1", "mybucket", "mykey", 0);
        let b = shard_key("u1", "mybucket", "mykey", 1);
        assert_ne!(a, b);
        assert_eq!(a, shard_key("u1", "mybucket", "mykey", 0));
    }

    /// #149: `shard_key` output embeds the user's bucket and key
    /// verbatim, and used to go straight into a URL path segment
    /// unescaped. actix-web fully percent-*decodes* path parameters, so
    /// a raw `?`, `#`, or `%` there could be reinterpreted as a query
    /// string, a fragment, or a different literal byte sequence by the
    /// receiving peer -- this is the invariant the percent-encoding fix
    /// in `put_shard`/`get_shard` relies on: percent-encode then
    /// percent-decode must always reconstruct the exact original bytes,
    /// for every character that caused the original bug plus a literal
    /// `/` (also reserved, and not even part of the original report).
    #[test]
    fn percent_encoding_a_shard_key_round_trips_through_decoding() {
        for key in ["victim?x", "victim#x", "victim%3Fx", "a b", "a/b/c", "100%"] {
            let sk = shard_key("u1", "bucket", key, 0);
            let encoded = utf8_percent_encode(&sk, NON_ALPHANUMERIC).to_string();
            // Every byte that isn't alphanumeric became a `%XX` escape, so
            // none of the raw reserved characters that caused the
            // original bug (`?`, `#`, a literal `%`, space, `/`) survive
            // as themselves in the encoded form -- nothing for a URL
            // parser or path router to reinterpret.
            for reserved in ['?', '#', ' ', '/'] {
                assert!(!encoded.contains(reserved), "encoded form still contains raw {reserved:?}: {encoded}");
            }
            let decoded = percent_encoding::percent_decode_str(&encoded)
                .decode_utf8()
                .unwrap();
            assert_eq!(decoded, sk, "round-trip mismatch for key {key:?}");
        }
    }
}
