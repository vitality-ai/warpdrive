//! Group commit: batches many concurrent callers' durability requirement
//! into as few real fsync syscalls as possible, instead of one fsync per
//! writer serializing every writer behind real disk latency individually.
//!
//! **Why this exists.** Every write path in this project (`local_store.rs`,
//! `location_store.rs`, `content_location_store.rs`, `shard_meta_store.rs`)
//! historically called neither `fsync` nor `fdatasync` anywhere: a write
//! was acknowledged to the client as soon as it was in the OS page cache,
//! with no guarantee it would survive a kernel panic or power loss before
//! some *unrelated* future writeback happened to flush it. A direct
//! per-write `sync_data()` fix was measured (docs/PERF-JOURNAL.md,
//! 2026-10-21) to cost 82-89% of throughput -- enough to drop this
//! project's whole cluster below the very baseline (MinIO) it was being
//! compared against. That's the wrong tradeoff: durability was never
//! supposed to be optional, but neither was it supposed to require paying
//! one full fsync latency *per request* when ten other requests' writes
//! already landed in the same window and could have shared the cost of
//! one.
//!
//! Group commit is the standard fix for exactly this shape of problem
//! (every real WAL-based database does some form of it): many concurrent
//! writers to the *same underlying file* collapse onto a single `fsync`
//! call per batch, with every writer in the batch correctly told "durable"
//! once that one call returns -- not before.
//!
//! **Why this is correct**, not just faster: each writer calls `ticket()`
//! immediately after its own write syscall returns (so the ticket number
//! is only ever handed out once the bytes are at least in the page cache,
//! on the issuing thread, strictly before that thread can call
//! `wait_for_durable`). The first thread to reach `wait_for_durable` while
//! no sync is in flight becomes the leader: it snapshots the *current*
//! highest ticket (`next_ticket`), which by definition already reflects
//! every ticket handed out before this snapshot -- including every other
//! thread now blocked behind the same gate, since each of *their* tickets
//! was also assigned before they ever got here. It then runs one real
//! `sync()`, which (being a single `fsync`-class syscall on this file
//! descriptor, issued strictly after every included writer's own write
//! syscall returned) is guaranteed to cover every one of those writes.
//! Every thread released from a subsequent lock acquisition re-checks
//! `synced_through` against its own ticket: if the batch it just missed
//! didn't cover it (only possible if it got its ticket *after* the leader
//! already snapshotted), it simply becomes the next leader and syncs
//! again -- which is always guaranteed, trivially, to cover at least its
//! own ticket.

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use parking_lot::Mutex;

/// Anything `wait_for_durable` can call a real flush-to-stable-storage on.
/// A trait, not a bare `&File`, so a test can supply a fake that counts
/// calls and introduces artificial delay -- the only deterministic way to
/// actually prove batching happens, instead of asserting against real
/// disk timing (slow and flaky to depend on in a unit test).
pub trait Syncable {
    fn sync(&self) -> io::Result<()>;
}

impl Syncable for std::fs::File {
    fn sync(&self) -> io::Result<()> {
        self.sync_data()
    }
}

/// One instance per independent on-disk file whose writers want batched
/// durability. Never shared across two different files -- syncing file
/// A's descriptor says nothing about whether file B's dirty pages reached
/// disk.
pub struct GroupCommit {
    next_ticket: AtomicU64,
    synced_through: AtomicU64,
    gate: Mutex<()>,
}

impl GroupCommit {
    pub fn new() -> Self {
        Self {
            next_ticket: AtomicU64::new(0),
            synced_through: AtomicU64::new(0),
            gate: Mutex::new(()),
        }
    }

    /// Call once, immediately after this thread's own write to the file
    /// has returned (i.e. the bytes are at least in the OS page cache).
    /// The returned ticket is this write's position in commit order --
    /// pass it to `wait_for_durable` to block until a real sync is
    /// guaranteed to have covered it.
    pub fn ticket(&self) -> u64 {
        self.next_ticket.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Blocks until `ticket`'s write is durable on stable storage. See
    /// the module doc for why this is correct under concurrent callers,
    /// not just fast.
    pub fn wait_for_durable<S: Syncable>(&self, ticket: u64, syncable: &S) -> io::Result<()> {
        if self.synced_through.load(Ordering::Acquire) >= ticket {
            return Ok(());
        }
        // Blocks here (a real OS-level park, not a spin) while another
        // thread's sync is in flight. Whoever gets the gate next re-checks
        // below before doing any I/O of its own.
        let _guard = self.gate.lock();
        if self.synced_through.load(Ordering::Acquire) >= ticket {
            return Ok(());
        }
        // Snapshot every ticket issued so far -- including this thread's
        // own, trivially, and every other thread now blocked on this same
        // gate, since each of their tickets was assigned before they ever
        // reached the lock above. The sync call below starts strictly
        // after this read, so it's guaranteed to cover all of them.
        let target = self.next_ticket.load(Ordering::Acquire);
        syncable.sync()?;
        self.synced_through.fetch_max(target, Ordering::AcqRel);
        Ok(())
    }
}

impl Default for GroupCommit {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    struct SlowCountingSync {
        calls: AtomicU64,
        delay: Duration,
    }

    impl Syncable for SlowCountingSync {
        fn sync(&self) -> io::Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            thread::sleep(self.delay);
            Ok(())
        }
    }

    /// The whole point of group commit: many concurrent waiters on a slow
    /// "disk" must collapse into far fewer real sync calls than there are
    /// waiters, not one each.
    #[test]
    fn concurrent_waiters_collapse_into_far_fewer_real_syncs() {
        let gc = Arc::new(GroupCommit::new());
        let sync = Arc::new(SlowCountingSync {
            calls: AtomicU64::new(0),
            delay: Duration::from_millis(20),
        });
        let threads = 50;
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let gc = Arc::clone(&gc);
                let sync = Arc::clone(&sync);
                thread::spawn(move || {
                    let ticket = gc.ticket();
                    gc.wait_for_durable(ticket, &*sync).unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let calls = sync.calls.load(Ordering::SeqCst);
        assert!(
            calls < threads as u64,
            "expected batching to reduce real sync calls well below {threads}, got {calls}"
        );
        assert!(calls >= 1);
    }

    /// Correctness, not just "fewer calls": every single ticket issued
    /// must actually return `Ok` -- batching must never silently skip
    /// someone's durability requirement.
    #[test]
    fn every_ticket_eventually_reports_durable_even_under_contention() {
        let gc = Arc::new(GroupCommit::new());
        let sync = Arc::new(SlowCountingSync {
            calls: AtomicU64::new(0),
            delay: Duration::from_millis(5),
        });
        let threads = 30;
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let gc = Arc::clone(&gc);
                let sync = Arc::clone(&sync);
                thread::spawn(move || -> io::Result<()> {
                    let ticket = gc.ticket();
                    gc.wait_for_durable(ticket, &*sync)
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap().unwrap();
        }
    }

    /// A single, uncontended caller must still get exactly one real sync
    /// -- group commit must never *skip* a sync just because it's the
    /// only one waiting.
    #[test]
    fn a_single_caller_with_no_contention_still_gets_synced() {
        let gc = GroupCommit::new();
        let sync = SlowCountingSync { calls: AtomicU64::new(0), delay: Duration::from_millis(0) };
        let ticket = gc.ticket();
        gc.wait_for_durable(ticket, &sync).unwrap();
        assert_eq!(sync.calls.load(Ordering::SeqCst), 1);
    }

    /// A sync failure must be reported to whichever caller actually hit
    /// it (e.g. disk full), not silently swallowed or hidden behind a
    /// stale "already synced" state.
    #[test]
    fn a_sync_error_is_propagated_to_the_caller_that_hit_it() {
        struct FailingSync;
        impl Syncable for FailingSync {
            fn sync(&self) -> io::Result<()> {
                Err(io::Error::new(io::ErrorKind::Other, "disk full"))
            }
        }
        let gc = GroupCommit::new();
        let ticket = gc.ticket();
        assert!(gc.wait_for_durable(ticket, &FailingSync).is_err());
    }

    /// A later caller must be able to recover and succeed even after an
    /// earlier caller's sync attempt failed -- one bad fsync shouldn't
    /// wedge every future ticket into permanent failure.
    #[test]
    fn a_later_caller_can_still_succeed_after_an_earlier_sync_failure() {
        struct OnceFailingSync {
            calls: AtomicU64,
        }
        impl Syncable for OnceFailingSync {
            fn sync(&self) -> io::Result<()> {
                if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    Err(io::Error::new(io::ErrorKind::Other, "transient"))
                } else {
                    Ok(())
                }
            }
        }
        let gc = GroupCommit::new();
        let sync = OnceFailingSync { calls: AtomicU64::new(0) };
        let first = gc.ticket();
        assert!(gc.wait_for_durable(first, &sync).is_err());
        let second = gc.ticket();
        assert!(gc.wait_for_durable(second, &sync).is_ok());
    }
}
