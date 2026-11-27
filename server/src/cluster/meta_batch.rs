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
use std::sync::{Arc, LazyLock};
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

pub fn enabled() -> bool {
    static ON: LazyLock<bool> = LazyLock::new(|| !std::env::var("WARPDRIVE_META_BATCH").is_ok_and(|v| v.trim().eq_ignore_ascii_case("off")));
    *ON
}

struct Pending {
    req: MetaRowRequest,
    reply: oneshot::Sender<RowReply>,
}

static QUEUES: LazyLock<parking_lot::Mutex<HashMap<String, mpsc::UnboundedSender<Pending>>>> = LazyLock::new(Default::default);

/// Sends `req` to `peer` in its next batch and returns the peer's answer.
pub async fn send(client: &reqwest::Client, peer: &str, req: MetaRowRequest) -> RowReply {
    let (reply, answer) = oneshot::channel();
    let tx = {
        let mut queues = QUEUES.lock();
        queues
            .entry(peer.to_string())
            .or_insert_with(|| {
                let (tx, rx) = mpsc::unbounded_channel();
                super::bridge::spawn(peer_loop(client.clone(), peer.to_string(), rx));
                tx
            })
            .clone()
    };
    if tx.send(Pending { req, reply }).is_err() {
        return RowReply::Failed;
    }
    answer.await.unwrap_or(RowReply::Failed)
}

fn approx_bytes(r: &MetaRowRequest) -> usize {
    r.key.len() + 16 + r.payload.as_ref().map_or(0, |p| p.0.len())
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
            MetaRowRequest { key: "a/k".into(), seq: 5, payload: Some(warp_engine::Payload(bytes.clone())) },
            MetaRowRequest { key: "b/k".into(), seq: 6, payload: None },
        ];
        let wire = rmp_serde::to_vec_named(&rows).unwrap();
        assert!(wire.windows(bytes.len()).any(|w| w == bytes.as_slice()), "the payload travels raw");
        let back: Vec<MetaRowRequest> = rmp_serde::from_slice(&wire).unwrap();
        assert_eq!((back[0].key.as_str(), back[0].seq, &back[0].payload), ("a/k", 5, &rows[0].payload));
        assert_eq!((back[1].key.as_str(), back[1].seq, back[1].payload.is_none()), ("b/k", 6, true));
    }

    #[test]
    fn replies_round_trip_as_messagepack() {
        let a = vec![RowReply::Applied, RowReply::Stale(42), RowReply::Failed];
        let b: Vec<RowReply> = rmp_serde::from_slice(&rmp_serde::to_vec_named(&a).unwrap()).unwrap();
        assert_eq!(a, b);
    }
}
