//! The engine's rows on this node (`warp_engine::RowStore`), and how a row
//! write reaches the cluster.
//!
//! A row is a key, a `seq` and an opaque payload. Nothing here reads a
//! payload: what the rows mean belongs to the layer that writes them.

use parking_lot::RwLock;
use std::sync::Arc;
use warp_engine::{Attachment, Payload, Row, RowStore, Watcher, WriteFuture};
use warp_meta_store::{Applied, Entry, RowOp, WarpMetaStore};

use super::meta_engine::{AnyEngine, MetaEngineKind};

/// The ordered row log on the metadata engine picked at startup
/// (`WARPDRIVE_META_ENGINE`, see `meta_engine`).
pub type RowLog = WarpMetaStore<Row, AnyEngine<Entry<Row>>>;

/// This node's copy of the rows, and the watchers to tell when one is
/// written. Every write on this node goes through here, so a watcher hears
/// about local writes and replicated ones alike.
pub struct NodeRows {
    log: RowLog,
    watchers: RwLock<Vec<(String, Watcher)>>,
    /// The commit thread, started on the first `commit`.
    committer: std::sync::OnceLock<std::sync::mpsc::Sender<CommitJob>>,
}

impl NodeRows {
    pub fn open(kind: MetaEngineKind, path: &str) -> std::io::Result<Self> {
        Ok(Self::new(WarpMetaStore::new(AnyEngine::open(kind, path, "rows")?)))
    }

    pub fn new(log: RowLog) -> Self {
        Self { log, watchers: RwLock::new(Vec::new()), committer: std::sync::OnceLock::new() }
    }

    /// The log, for reads, compaction and garbage collection.
    pub fn log(&self) -> &RowLog {
        &self.log
    }

    pub fn watch(&self, prefix: &str, on_write: Watcher) {
        self.watchers.write().push((prefix.to_string(), on_write));
    }

    fn written(&self, key: &str) {
        for (prefix, w) in self.watchers.read().iter() {
            if key.starts_with(prefix.as_str()) {
                w(key);
            }
        }
    }

    pub fn put(&self, key: String, row: Row) -> std::io::Result<Applied<Row>> {
        let out = self.log.put(key.clone(), row)?;
        self.written(&key);
        Ok(out)
    }

    pub fn delete(&self, key: String, seq: u64) -> std::io::Result<Applied<Row>> {
        let out = self.log.delete(key.clone(), seq)?;
        self.written(&key);
        Ok(out)
    }

    /// Many writes, durable together (one sync).
    pub fn apply_many(&self, ops: Vec<(String, RowOp<Row>)>) -> std::io::Result<Vec<Applied<Row>>> {
        let keys: Vec<String> = ops.iter().map(|(k, _)| k.clone()).collect();
        let out = self.log.apply_many(ops)?;
        for k in &keys {
            self.written(k);
        }
        Ok(out)
    }
}

/// One row write waiting for the commit thread.
pub struct CommitJob {
    key: String,
    op: RowOp<Row>,
    reply: tokio::sync::oneshot::Sender<Result<Applied<Row>, String>>,
}

/// The most row writes one commit applies.
const COMMIT_BATCH_MAX: usize = 1024;

impl NodeRows {
    /// Writes one row to this node's rows (a tombstone at `seq` when
    /// `payload` is `None`), durable when it returns, without blocking the
    /// caller's thread.
    ///
    /// Every caller's write goes to one commit thread. It takes all the
    /// writes waiting, applies them with one `apply_many` (one engine write,
    /// one sync), and answers each. Under load a batch holds many writes, so
    /// a write costs a channel send and a wakeup instead of a blocking-pool
    /// hand-off and its own place in the engine's write queue. Each write
    /// gets the answer `put` or `delete` would give it.
    pub async fn commit(self: &Arc<Self>, key: String, seq: u64, payload: Option<Payload>) -> Result<Applied<Row>, String> {
        let tx = self.committer.get_or_init(|| {
            let (tx, rx) = std::sync::mpsc::channel::<CommitJob>();
            // The thread holds the rows only weakly, so dropping the rows
            // (a test's store) ends it.
            let rows = Arc::downgrade(self);
            std::thread::Builder::new()
                .name("wd-row-commit".into())
                .spawn(move || commit_loop(rows, rx))
                .expect("starting the row commit thread");
            tx
        });
        let op = match payload {
            Some(payload) => RowOp::Put(Row { seq, payload }),
            None => RowOp::Delete(seq),
        };
        let (reply, answer) = tokio::sync::oneshot::channel();
        tx.send(CommitJob { key, op, reply }).map_err(|_| "the row commit thread has stopped".to_string())?;
        answer.await.map_err(|_| "the row commit thread dropped a write".to_string())?
    }
}

fn commit_loop(rows: std::sync::Weak<NodeRows>, rx: std::sync::mpsc::Receiver<CommitJob>) {
    while let Ok(first) = rx.recv() {
        let mut jobs = vec![first];
        while jobs.len() < COMMIT_BATCH_MAX {
            match rx.try_recv() {
                Ok(j) => jobs.push(j),
                Err(_) => break,
            }
        }
        let (ops, replies): (Vec<(String, RowOp<Row>)>, Vec<_>) = jobs.into_iter().map(|j| ((j.key, j.op), j.reply)).unzip();
        let Some(rows) = rows.upgrade() else {
            for reply in replies {
                let _ = reply.send(Err("this node's rows are closed".into()));
            }
            return;
        };
        match rows.apply_many(ops) {
            Ok(results) => {
                for (reply, r) in replies.into_iter().zip(results) {
                    let _ = reply.send(Ok(r));
                }
            }
            Err(e) => {
                for reply in replies {
                    let _ = reply.send(Err(e.to_string()));
                }
            }
        }
    }
}

fn scan(log: &RowLog, prefix: &str, f: &mut dyn FnMut(&str, &Row)) {
    log.list_live(prefix, |k, r| f(k, r));
}

/// The rows as the cluster sees them: reads from this node's copy, writes
/// replicated to a write quorum.
pub struct ClusterRows(pub Arc<NodeRows>);

impl RowStore for ClusterRows {
    fn get(&self, key: &str) -> Option<Row> {
        self.0.log.get(key)
    }

    fn floor(&self, prefix: &str, key: &str) -> Option<(String, Row)> {
        self.0.log.floor_live(prefix, key)
    }

    fn scan(&self, prefix: &str, f: &mut dyn FnMut(&str, &Row)) {
        scan(&self.0.log, prefix, f)
    }

    fn write(&self, key: &str, seq: u64, payload: Option<Payload>) -> Result<(), String> {
        let state = super::bridge::state().ok_or("cluster state not installed")?;
        let peers = state.membership.peers();
        let required = super::coordinator::required_write_acks(state.ec.k(), state.ec.m());
        let key = key.to_string();
        let acked = super::bridge::run(move || async move { super::coordinator::replicate_row(&state, &peers, &key, seq, payload).await });
        if acked < required {
            return Err(format!("{acked}/{required} nodes stored the row"));
        }
        Ok(())
    }

    fn write_async(&self, key: String, seq: u64, payload: Option<Payload>) -> WriteFuture {
        Box::pin(async move {
            let state = super::bridge::state().ok_or("cluster state not installed")?;
            let peers = state.membership.peers();
            let required = super::coordinator::required_write_acks(state.ec.k(), state.ec.m());
            let acked = super::coordinator::replicate_row(&state, &peers, &key, seq, payload).await;
            if acked < required {
                return Err(format!("{acked}/{required} nodes stored the row"));
            }
            Ok(())
        })
    }

    /// One round when every peer holds a shard: each gets its shard in the
    /// row's message and acknowledges both, so a quorum of acks is a quorum
    /// of rows and of shards. Otherwise (more nodes than shards) the shards
    /// go first, to a quorum of their holders, then the row.
    fn write_with(&self, key: String, seq: u64, payload: Option<Payload>, shards: Vec<Attachment>) -> WriteFuture {
        if shards.is_empty() {
            return self.write_async(key, seq, payload);
        }
        Box::pin(async move {
            let state = super::bridge::state().ok_or("cluster state not installed")?;
            let peers = state.membership.peers();
            let required = super::coordinator::required_write_acks(state.ec.k(), state.ec.m());
            let norm = |p: &str| p.trim().trim_end_matches('/').to_string();
            let holders: std::collections::HashSet<String> = shards.iter().map(|a| norm(&a.peer)).collect();
            let one_round = holders.len() == shards.len() && peers.iter().all(|p| holders.contains(&norm(p)));
            if !one_round {
                let puts = shards.into_iter().map(|a| {
                    let client = std::sync::Arc::clone(&state.peer_client);
                    async move {
                        let (user, bucket) = super::coordinator::split_space(&a.space).ok_or("unreadable shard space")?;
                        client.put_shard(&a.peer, user, bucket, &a.key, a.index, a.data.0).await.map_err(|e| e.to_string())
                    }
                });
                let stored = futures::future::join_all(puts).await.into_iter().filter(|r| r.is_ok()).count();
                if stored < required {
                    return Err(format!("{stored}/{required} nodes stored a shard"));
                }
                let acked = super::coordinator::replicate_row(&state, &peers, &key, seq, payload).await;
                return if acked < required { Err(format!("{acked}/{required} nodes stored the row")) } else { Ok(()) };
            }
            let acked = super::coordinator::replicate_row_with(&state, &peers, &key, seq, payload, Some(std::sync::Arc::new(shards))).await;
            if acked < required {
                return Err(format!("{acked}/{required} nodes stored the row and its shard"));
            }
            Ok(())
        })
    }

    fn write_with_blocking(&self, key: &str, seq: u64, payload: Option<Payload>, shards: Vec<Attachment>) -> Result<(), String> {
        let fut = self.write_with(key.to_string(), seq, payload, shards);
        super::bridge::run(move || fut)
    }

    fn watch(&self, prefix: &str, on_write: Watcher) {
        self.0.watch(prefix, on_write)
    }
}

/// The rows on this node alone: writes apply locally. For tests.
pub struct LocalRows(pub Arc<NodeRows>);

impl RowStore for LocalRows {
    fn get(&self, key: &str) -> Option<Row> {
        self.0.log.get(key)
    }

    fn floor(&self, prefix: &str, key: &str) -> Option<(String, Row)> {
        self.0.log.floor_live(prefix, key)
    }

    fn scan(&self, prefix: &str, f: &mut dyn FnMut(&str, &Row)) {
        scan(&self.0.log, prefix, f)
    }

    fn write(&self, key: &str, seq: u64, payload: Option<Payload>) -> Result<(), String> {
        let res = match payload {
            Some(payload) => self.0.put(key.to_string(), Row { seq, payload }),
            None => self.0.delete(key.to_string(), seq),
        }
        .map_err(|e| e.to_string())?;
        match res {
            Applied::Yes { .. } => Ok(()),
            other => Err(format!("row write refused: {other:?}")),
        }
    }

    fn write_async(&self, key: String, seq: u64, payload: Option<Payload>) -> WriteFuture {
        Box::pin(std::future::ready(self.write(&key, seq, payload)))
    }

    fn watch(&self, prefix: &str, on_write: Watcher) {
        self.0.watch(prefix, on_write)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn local() -> (tempfile::TempDir, LocalRows) {
        let dir = tempfile::tempdir().unwrap();
        let node = NodeRows::open(MetaEngineKind::Bitcask, &dir.path().join("rows.log").to_string_lossy()).unwrap();
        (dir, LocalRows(Arc::new(node)))
    }

    #[test]
    fn rows_are_ordered_by_seq_and_opaque() {
        let (_d, rows) = local();
        rows.write("a/1", 10, Some(Payload(b"ten".to_vec()))).unwrap();
        assert!(rows.write("a/1", 9, Some(Payload(b"nine".to_vec()))).is_err(), "an older write is refused");
        assert_eq!(rows.get("a/1").unwrap().payload.0, b"ten");
        rows.write("a/2", 11, Some(Payload(vec![0, 255]))).unwrap();
        rows.write("b/1", 12, Some(Payload(vec![]))).unwrap();
        let mut seen = Vec::new();
        rows.scan("a/", &mut |k, r| seen.push((k.to_string(), r.seq)));
        assert_eq!(seen, vec![("a/1".to_string(), 10), ("a/2".to_string(), 11)]);
        assert_eq!(rows.floor("a/", "a/9").unwrap().0, "a/2");
        rows.write("a/2", 13, None).unwrap();
        assert!(rows.get("a/2").is_none(), "a tombstone hides the row");
        assert!(rows.write("a/2", 12, Some(Payload(vec![1]))).is_err(), "an older write can't revive it");
    }

    #[tokio::test]
    async fn commits_from_many_writers_are_ordered_and_durable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rows.log").to_string_lossy().into_owned();
        {
            let node = Arc::new(NodeRows::open(MetaEngineKind::Bitcask, &path).unwrap());
            let writes = (0..200u64).map(|i| {
                let node = Arc::clone(&node);
                async move { node.commit(format!("k/{}", i % 10), 1000 + i, Some(Payload(i.to_be_bytes().to_vec()))).await }
            });
            let results = futures::future::join_all(writes).await;
            assert!(results.iter().all(|r| r.is_ok()));
            assert!(matches!(node.commit("k/0".into(), 1, Some(Payload(vec![]))).await.unwrap(), Applied::Stale { .. }), "an older seq is stale");
            node.commit("k/1".into(), 5000, None).await.unwrap();
        }
        let node = NodeRows::open(MetaEngineKind::Bitcask, &path).unwrap();
        // Each key holds its newest write, after a reopen.
        assert_eq!(node.log().get("k/0").unwrap().seq, 1190);
        assert!(node.log().get("k/1").is_none(), "the tombstone survived");
    }

    #[test]
    fn watchers_hear_every_write_under_their_prefix() {
        let (_d, rows) = local();
        let hits = Arc::new(AtomicUsize::new(0));
        let h = Arc::clone(&hits);
        rows.watch("bkt/", Arc::new(move |_| {
            h.fetch_add(1, Ordering::SeqCst);
        }));
        rows.write("bkt/x", 1, Some(Payload(vec![1]))).unwrap();
        rows.write("ver/x", 2, Some(Payload(vec![1]))).unwrap();
        // A write arriving from a peer lands through NodeRows directly.
        rows.0.apply_many(vec![("bkt/y".into(), RowOp::Put(Row { seq: 3, payload: Payload(vec![]) }))]).unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }
}
