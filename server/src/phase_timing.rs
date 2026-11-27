//! Phase timers: atomic counters cheap enough to leave on, read from
//! `/cluster/_internal/timing`. No layer owns them, so any layer can record
//! its phases without reaching into another.

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

/// Where an S3 PUT's time goes on the node that serves it.
#[derive(Default)]
pub struct PutPhases {
    /// Reading the request body from the client.
    pub body: Phase,
    /// Writing the body to the store (shards, or inline).
    pub blob: Phase,
    /// Writing the object's metadata rows.
    pub meta: Phase,
    /// The whole handler.
    pub total: Phase,
}

impl PutPhases {
    pub fn summary(&self) -> String {
        format!(
            "s3_put_n={} body_avg_ms={:.3} blob_avg_ms={:.3} meta_avg_ms={:.3} total_avg_ms={:.3}\n",
            self.total.count(),
            self.body.avg_ms(),
            self.blob.avg_ms(),
            self.meta.avg_ms(),
            self.total.avg_ms()
        )
    }
}

pub static S3_PUT: std::sync::LazyLock<PutPhases> = std::sync::LazyLock::new(PutPhases::default);
