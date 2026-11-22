//! Runs cluster operations (async) from the S3 handlers' synchronous
//! storage and metadata calls.
//!
//! The S3 handlers call `Storage` and `MetadataStorage` synchronously. On a
//! cluster, those calls replicate over the network, which is async code. A
//! handler can't `block_on` a future itself: it runs inside actix's
//! runtime. So each call is handed to a small pool of plain threads. A pool
//! thread builds the future and drives it with `block_on` on one shared
//! multi-threaded runtime, whose workers keep background network tasks
//! (pooled connections) moving. The handler waits for the answer.
//!
//! The future is built on the pool thread, so it doesn't have to be `Send`:
//! only the closure that builds it and the result cross threads.

use actix_web::web;
use std::future::Future;
use std::sync::mpsc;
use std::sync::{LazyLock, OnceLock};

use super::coordinator::ClusterState;

type Job = Box<dyn FnOnce(&tokio::runtime::Runtime) + Send>;

static RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .thread_name("wd-bridge-rt")
        .enable_all()
        .build()
        .expect("failed to build the cluster bridge runtime")
});

/// Threads that can wait on a cluster operation at once. A handler blocked
/// here holds one. Default 64, `WARPDRIVE_BRIDGE_THREADS` overrides.
fn pool_size() -> usize {
    std::env::var("WARPDRIVE_BRIDGE_THREADS").ok().and_then(|v| v.parse().ok()).filter(|n| *n > 0).unwrap_or(64)
}

static POOL: LazyLock<mpsc::Sender<Job>> = LazyLock::new(|| {
    let (tx, rx) = mpsc::channel::<Job>();
    let rx = std::sync::Arc::new(std::sync::Mutex::new(rx));
    for i in 0..pool_size() {
        let rx = std::sync::Arc::clone(&rx);
        std::thread::Builder::new()
            .name(format!("wd-bridge-{i}"))
            .spawn(move || loop {
                let job = match rx.lock() {
                    Ok(guard) => match guard.recv() {
                        Ok(job) => job,
                        Err(_) => return,
                    },
                    Err(_) => return,
                };
                job(&RUNTIME);
            })
            .expect("failed to start a cluster bridge thread");
    }
    tx
});

/// Runs `fut` as a task on the bridge runtime. Awaiting the handle does not
/// block the caller's thread, unlike `run`.
pub fn spawn<T: Send + 'static>(fut: impl Future<Output = T> + Send + 'static) -> tokio::task::JoinHandle<T> {
    RUNTIME.spawn(fut)
}

static STATE: OnceLock<web::Data<ClusterState>> = OnceLock::new();

/// Makes this node's cluster state available to the cluster stores. Called
/// once at startup.
pub fn install(state: web::Data<ClusterState>) {
    let _ = STATE.set(state);
}

/// This node's cluster state, once `install` has run.
pub fn state() -> Option<web::Data<ClusterState>> {
    STATE.get().cloned()
}

/// Runs the future `make` builds, on a bridge thread, and waits for its
/// output. Must not be called from a bridge thread itself.
pub fn run<T, Fut>(make: impl FnOnce() -> Fut + Send + 'static) -> T
where
    T: Send + 'static,
    Fut: Future<Output = T> + 'static,
{
    let (tx, rx) = mpsc::sync_channel(1);
    POOL.send(Box::new(move |rt: &tokio::runtime::Runtime| {
        let _ = tx.send(rt.block_on(make()));
    }))
    .expect("cluster bridge pool stopped");
    rx.recv().expect("cluster bridge job dropped its result")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_a_non_send_future_and_returns_its_output() {
        let out = run(|| async {
            let local = std::rc::Rc::new(41);
            tokio::task::yield_now().await;
            *local + 1
        });
        assert_eq!(out, 42);
    }

    #[test]
    fn many_callers_at_once_all_get_their_own_answer() {
        let handles: Vec<_> = (0..32)
            .map(|i| std::thread::spawn(move || run(move || async move {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                i * 2
            })))
            .collect();
        let got: Vec<i32> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(got, (0..32).map(|i| i * 2).collect::<Vec<_>>());
    }
}
