//! Hybrid logical clock for ordering record writes (RFC 6).
//!
//! Every location record and tombstone carries a `seq` from this clock. A
//! node applies a write only if its `seq` is higher than what the node
//! already has for that key, so every node ends on the same record no
//! matter what order the writes arrive in.
//!
//! A stamp is the wall clock in nanoseconds, raised above every stamp this
//! node has issued or seen. So a node that has seen a record always stamps
//! a later write to the same key higher, even if its own clock is behind.

use std::sync::atomic::{AtomicU64, Ordering};

static LAST: AtomicU64 = AtomicU64::new(0);

fn wall_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// A new stamp, higher than every stamp this node has issued or observed.
pub fn now() -> u64 {
    let wall = wall_nanos();
    let mut last = LAST.load(Ordering::Relaxed);
    loop {
        let next = wall.max(last + 1);
        match LAST.compare_exchange_weak(last, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return next,
            Err(seen) => last = seen,
        }
    }
}

/// Records that `seq` exists, so later stamps from this node are higher.
pub fn observe(seq: u64) {
    LAST.fetch_max(seq, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps_strictly_increase() {
        let a = now();
        let b = now();
        assert!(b > a);
    }

    #[test]
    fn a_stamp_after_observing_a_future_one_is_higher() {
        let future = wall_nanos() + 3_600_000_000_000;
        observe(future);
        assert!(now() > future);
    }
}
