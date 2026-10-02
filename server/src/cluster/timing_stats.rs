//! Diagnostic phase-level timing for `cluster_put_object`, added
//! specifically to answer "where are we slow if we're not saturating
//! anywhere" — a GCP load test showed flat throughput at low CPU
//! utilization even as client concurrency increased 4x, the classic
//! signature of a concurrency limit somewhere in the pipeline rather than
//! a compute bottleneck. Atomic counters, not per-request logging, so
//! collecting this data doesn't itself perturb the measurement.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

#[derive(Default)]
pub struct Phase {
    total_ns: AtomicU64,
    count: AtomicU64,
    max_ns: AtomicU64,
}

impl Phase {
    pub fn record(&self, d: Duration) {
        let ns = d.as_nanos() as u64;
        self.total_ns.fetch_add(ns, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
        self.max_ns.fetch_max(ns, Ordering::Relaxed);
    }

    pub fn avg_ms(&self) -> f64 {
        let count = self.count.load(Ordering::Relaxed);
        if count == 0 {
            return 0.0;
        }
        (self.total_ns.load(Ordering::Relaxed) as f64 / count as f64) / 1_000_000.0
    }

    pub fn max_ms(&self) -> f64 {
        self.max_ns.load(Ordering::Relaxed) as f64 / 1_000_000.0
    }

    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }
}

#[derive(Default)]
pub struct TimingStats {
    pub encode: Phase,
    pub shard_fanout: Phase,
    pub location_replicate: Phase,
    pub total: Phase,
}

impl TimingStats {
    pub fn summary(&self) -> String {
        format!(
            "n={}\n\
             encode_avg_ms={:.3} encode_max_ms={:.3}\n\
             shard_fanout_avg_ms={:.3} shard_fanout_max_ms={:.3}\n\
             location_replicate_avg_ms={:.3} location_replicate_max_ms={:.3}\n\
             total_avg_ms={:.3} total_max_ms={:.3}\n",
            self.total.count(),
            self.encode.avg_ms(),
            self.encode.max_ms(),
            self.shard_fanout.avg_ms(),
            self.shard_fanout.max_ms(),
            self.location_replicate.avg_ms(),
            self.location_replicate.max_ms(),
            self.total.avg_ms(),
            self.total.max_ms(),
        )
    }
}
