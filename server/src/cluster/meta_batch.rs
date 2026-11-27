//! Metadata rows sent to peers in batches.
//!
//! A row write goes to every peer. Sent one HTTP request per row, a node
//! spends more on requests than on the rows: at 5k small PUTs a second each
//! node handled about 20k requests a second and 9 context switches per row.
//! Instead, each peer has a sender task. Rows for that peer wait in its
//! queue, and the task sends everything waiting as one request (MessagePack,
//! so inline bytes travel raw), with up to `MAX_IN_FLIGHT` requests out at
//! once. While those are out, more rows queue, so batches grow with load and
//! cost nothing extra when it's light. The peer applies a batch through its
//! commit thread (`NodeRows::commit`), so the whole batch is
//! durable with one sync there too, and answers each row as `/meta` would.
//!
//! `WARPDRIVE_META_BATCH=off` sends one request per row (`/meta`) instead.

use actix_web::{web, HttpResponse};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, Semaphore};

use super::coordinator::MetaRowRequest;

/// Most rows in one request.
const MAX_ROWS: usize = 1024;
/// A batch closes once its rows hold this many bytes (roughly: keys and
/// inline values), so one request stays a few MB.
const MAX_BYTES: usize = 4 << 20;
/// Requests in flight to one peer.
const MAX_IN_FLIGHT: usize = 4;

/// A peer's answer for one row.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum RowReply {
    Applied,
    /// The peer holds a newer write for the key, at this seq.
    Stale(u64),
    Failed,
}

/// How a row write reaches another node. Chosen once at startup
/// ([`from_env`]) and kept in `ClusterState`.
#[async_trait::async_trait]
pub trait RowTransport: Send + Sync {
    async fn send(&self, peer: &str, req: MetaRowRequest) -> RowReply;
}

/// Batched (the default), or one request per row with
/// `WARPDRIVE_META_BATCH=off`.
pub fn from_env(client: reqwest::Client) -> std::sync::Arc<dyn RowTransport> {
    if std::env::var("WARPDRIVE_META_BATCH").is_ok_and(|v| v.trim().eq_ignore_ascii_case("off")) {
        log::info!("Row writes go to peers one request per row (WARPDRIVE_META_BATCH=off)");
        std::sync::Arc::new(PerRow { client })
    } else {
        std::sync::Arc::new(Batched { client, queues: Default::default() })
    }
}

/// One HTTP request per row (`/cluster/_internal/meta`).
pub struct PerRow {
    client: reqwest::Client,
}

#[async_trait::async_trait]
impl RowTransport for PerRow {
    async fn send(&self, peer: &str, req: MetaRowRequest) -> RowReply {
        let url = format!("{}/cluster/_internal/meta", peer.trim_end_matches('/'));
        match self.client.post(url).json(&req).send().await {
            Ok(r) if r.status().is_success() => RowReply::Applied,
            Ok(r) if r.status() == reqwest::StatusCode::CONFLICT => match r.json::<super::coordinator::StaleReply>().await {
                Ok(s) => RowReply::Stale(s.current_seq),
                Err(e) => {
                    log::warn!("row write to {peer}: unreadable 409 body: {e}");
                    RowReply::Failed
                }
            },
            Ok(r) => {
                log::warn!("row write to {peer} returned {}", r.status());
                RowReply::Failed
            }
            Err(e) => {
                log::warn!("row write to {peer} failed: {e}");
                RowReply::Failed
            }
        }
    }
}

struct Pending {
    req: MetaRowRequest,
    reply: oneshot::Sender<RowReply>,
}

/// Rows to a peer ride in that peer's next batch: one queue and sender
/// task per peer.
pub struct Batched {
    client: reqwest::Client,
    queues: parking_lot::Mutex<HashMap<String, mpsc::UnboundedSender<Pending>>>,
}

#[async_trait::async_trait]
impl RowTransport for Batched {
    async fn send(&self, peer: &str, req: MetaRowRequest) -> RowReply {
        let (reply, answer) = oneshot::channel();
        let tx = {
            let mut queues = self.queues.lock();
            queues
                .entry(peer.to_string())
                .or_insert_with(|| {
                    let (tx, rx) = mpsc::unbounded_channel();
                    super::bridge::spawn(peer_loop(self.client.clone(), peer.to_string(), rx));
                    tx
                })
                .clone()
        };
        if tx.send(Pending { req, reply }).is_err() {
            return RowReply::Failed;
        }
        answer.await.unwrap_or(RowReply::Failed)
    }
}

fn approx_bytes(r: &MetaRowRequest) -> usize {
    r.key.len() + 16 + r.payload.as_ref().map_or(0, |p| p.0.len()) + r.shard.as_ref().map_or(0, |a| a.data.0.len() + a.key.len() + a.space.len())
}

async fn peer_loop(client: reqwest::Client, peer: String, mut rx: mpsc::UnboundedReceiver<Pending>) {
    let slots = Arc::new(Semaphore::new(MAX_IN_FLIGHT));
    let url = format!("{}/cluster/_internal/meta_batch", peer.trim_end_matches('/'));
    loop {
        // Waits for a free slot first: rows that arrive meanwhile queue and
        // go out together in the next batch.
        let Ok(slot) = Arc::clone(&slots).acquire_owned().await else { return };
        let Some(first) = rx.recv().await else { return };
        let mut bytes = approx_bytes(&first.req);
        let mut batch = vec![first];
        while batch.len() < MAX_ROWS && bytes < MAX_BYTES {
            match rx.try_recv() {
                Ok(p) => {
                    bytes += approx_bytes(&p.req);
                    batch.push(p);
                }
                Err(_) => break,
            }
        }
        let (client, url) = (client.clone(), url.clone());
        tokio::spawn(async move {
            let _slot = slot;
            let (reqs, replies): (Vec<MetaRowRequest>, Vec<oneshot::Sender<RowReply>>) = batch.into_iter().map(|p| (p.req, p.reply)).unzip();
            let answers = post(&client, &url, &reqs).await;
            for (reply, a) in replies.into_iter().zip(answers) {
                let _ = reply.send(a);
            }
        });
    }
}

/// One batch request. A transport or decode failure fails every row in it.
async fn post(client: &reqwest::Client, url: &str, reqs: &[MetaRowRequest]) -> Vec<RowReply> {
    let failed = || vec![RowReply::Failed; reqs.len()];
    let body = match rmp_serde::to_vec_named(reqs) {
        Ok(b) => b,
        Err(e) => {
            log::warn!("metadata batch to {url}: encoding failed: {e}");
            return failed();
        }
    };
    let resp = match client.post(url).header("content-type", "application/msgpack").body(body).send().await {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            log::warn!("metadata batch of {} rows to {url} returned {}", reqs.len(), r.status());
            return failed();
        }
        Err(e) => {
            log::warn!("metadata batch of {} rows to {url} failed: {e}", reqs.len());
            return failed();
        }
    };
    match resp.bytes().await.map_err(|e| e.to_string()).and_then(|b| rmp_serde::from_slice::<Vec<RowReply>>(&b).map_err(|e| e.to_string())) {
        Ok(a) if a.len() == reqs.len() => a,
        Ok(a) => {
            log::warn!("metadata batch to {url}: {} answers for {} rows", a.len(), reqs.len());
            failed()
        }
        Err(e) => {
            log::warn!("metadata batch to {url}: unreadable answer: {e}");
            failed()
        }
    }
}

/// `/cluster/_internal/meta_batch`: applies each row through this node's
/// commit thread (so the batch shares a sync) and answers each.
pub async fn cluster_internal_meta_batch(body: web::Bytes, state: web::Data<super::coordinator::ClusterState>) -> HttpResponse {
    let Some(rows) = state.rows.get().cloned() else {
        return HttpResponse::ServiceUnavailable().body("this node has no rows (not started as a cluster node)");
    };
    let reqs: Vec<MetaRowRequest> = match rmp_serde::from_slice(&body) {
        Ok(r) => r,
        Err(e) => return HttpResponse::BadRequest().body(format!("unreadable metadata batch: {e}")),
    };
    let answers: Vec<RowReply> = futures::future::join_all(reqs.into_iter().map(|m| {
        let rows = Arc::clone(&rows);
        async move {
        if let Err(e) = super::coordinator::store_attached(m.shard).await {
            log::warn!("metadata batch row: storing its shard on this node failed: {e}");
            return RowReply::Failed;
        }
        match rows.commit(m.key, m.seq, m.payload).await {
            Ok(warp_meta_store::Applied::Yes { .. }) => RowReply::Applied,
            Ok(warp_meta_store::Applied::Stale { current_seq }) => RowReply::Stale(current_seq),
            Ok(warp_meta_store::Applied::VersionMismatch) => RowReply::Failed,
            Err(e) => {
                log::warn!("metadata batch row: applying on this node failed: {e}");
                RowReply::Failed
            }
        }
        }
    }))
    .await;
    match rmp_serde::to_vec_named(&answers) {
        Ok(b) => HttpResponse::Ok().content_type("application/msgpack").body(b),
        Err(e) => HttpResponse::InternalServerError().body(format!("encoding the answers failed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_and_tombstones_round_trip_as_messagepack_with_raw_payloads() {
        let bytes: Vec<u8> = (0..=255).collect();
        let rows = vec![
            MetaRowRequest { key: "a/k".into(), seq: 5, payload: Some(warp_engine::Payload(bytes.clone())), ..Default::default() },
            MetaRowRequest { key: "b/k".into(), seq: 6, payload: None, ..Default::default() },
        ];
        let wire = rmp_serde::to_vec_named(&rows).unwrap();
        assert!(wire.windows(bytes.len()).any(|w| w == bytes.as_slice()), "the payload travels raw");
        let back: Vec<MetaRowRequest> = rmp_serde::from_slice(&wire).unwrap();
        assert_eq!((back[0].key.as_str(), back[0].seq, &back[0].payload), ("a/k", 5, &rows[0].payload));
        assert_eq!((back[1].key.as_str(), back[1].seq, back[1].payload.is_none()), ("b/k", 6, true));
    }

    #[test]
    fn a_row_carries_its_shard_raw_and_never_the_other_peers_shards() {
        let data: Vec<u8> = (0..=255).rev().collect();
        let a = warp_engine::Attachment { peer: "p1".into(), space: "1:u:b".into(), key: "w".into(), index: 2, data: warp_engine::Payload(data.clone()) };
        let other = warp_engine::Attachment { index: 3, ..a.clone() };
        let row = MetaRowRequest { key: "k".into(), seq: 9, payload: None, shard: Some(a.clone()), shards: Some(std::sync::Arc::new(vec![a.clone(), other])) };
        let wire = rmp_serde::to_vec_named(&vec![row]).unwrap();
        assert!(wire.windows(data.len()).any(|w| w == data.as_slice()), "the shard travels raw");
        let back: Vec<MetaRowRequest> = rmp_serde::from_slice(&wire).unwrap();
        assert_eq!(back[0].shard.as_ref(), Some(&a));
        assert!(back[0].shards.is_none(), "every peer's shards stay on the sender");
        // A row without a shard reads as before.
        let plain = rmp_serde::to_vec_named(&MetaRowRequest { key: "k".into(), seq: 1, ..Default::default() }).unwrap();
        assert!(rmp_serde::from_slice::<MetaRowRequest>(&plain).unwrap().shard.is_none());
    }

    #[test]
    fn replies_round_trip_as_messagepack() {
        let a = vec![RowReply::Applied, RowReply::Stale(42), RowReply::Failed];
        let b: Vec<RowReply> = rmp_serde::from_slice(&rmp_serde::to_vec_named(&a).unwrap()).unwrap();
        assert_eq!(a, b);
    }
}
