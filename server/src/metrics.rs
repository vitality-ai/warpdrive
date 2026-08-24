use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

pub static GET: AtomicU64 = AtomicU64::new(0);
pub static PUT: AtomicU64 = AtomicU64::new(0);
pub static HEAD: AtomicU64 = AtomicU64::new(0);
pub static DELETE: AtomicU64 = AtomicU64::new(0);
pub static LIST: AtomicU64 = AtomicU64::new(0);
pub static DELETE_OBJECTS: AtomicU64 = AtomicU64::new(0);
pub static COPY: AtomicU64 = AtomicU64::new(0);
pub static MULTIPART: AtomicU64 = AtomicU64::new(0);

// Cumulative request duration (nanoseconds) and max duration seen, per op.
// Paired with the counters above this gives average latency (sum/count),
// matching pageserver's own remote_storage_s3_request_seconds pattern, so
// server-side (WarpDrive) and client-side (pageserver) latency views can be
// cross-checked the same way MinIO's TTFB histogram let us cross-check MinIO.
pub static GET_DURATION_NANOS: AtomicU64 = AtomicU64::new(0);
pub static PUT_DURATION_NANOS: AtomicU64 = AtomicU64::new(0);
pub static HEAD_DURATION_NANOS: AtomicU64 = AtomicU64::new(0);
pub static DELETE_DURATION_NANOS: AtomicU64 = AtomicU64::new(0);

pub static GET_MAX_NANOS: AtomicU64 = AtomicU64::new(0);
pub static PUT_MAX_NANOS: AtomicU64 = AtomicU64::new(0);
pub static HEAD_MAX_NANOS: AtomicU64 = AtomicU64::new(0);
pub static DELETE_MAX_NANOS: AtomicU64 = AtomicU64::new(0);

/// Increment a counter. Compiled to nothing without the `op-counters` feature.
#[macro_export]
macro_rules! count {
    ($counter:expr) => {
        #[cfg(feature = "op-counters")]
        $counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    };
}

/// Record a request's wall-clock duration into a (sum, max) atomic pair.
/// Compiled to nothing without the `op-counters` feature.
#[cfg(feature = "op-counters")]
pub fn record_duration(sum: &AtomicU64, max: &AtomicU64, d: Duration) {
    let nanos = d.as_nanos().min(u64::MAX as u128) as u64;
    sum.fetch_add(nanos, Ordering::Relaxed);
    max.fetch_max(nanos, Ordering::Relaxed);
}

#[cfg(not(feature = "op-counters"))]
pub fn record_duration(_sum: &AtomicU64, _max: &AtomicU64, _d: Duration) {}

pub fn capture() -> OpCounts {
    OpCounts {
        get:            GET.load(Ordering::Relaxed),
        put:            PUT.load(Ordering::Relaxed),
        head:           HEAD.load(Ordering::Relaxed),
        delete:         DELETE.load(Ordering::Relaxed),
        list:           LIST.load(Ordering::Relaxed),
        delete_objects: DELETE_OBJECTS.load(Ordering::Relaxed),
        copy:           COPY.load(Ordering::Relaxed),
        multipart:      MULTIPART.load(Ordering::Relaxed),
    }
}

pub fn capture_latencies() -> OpLatencies {
    OpLatencies {
        get:    latency_stats(&GET, &GET_DURATION_NANOS, &GET_MAX_NANOS),
        put:    latency_stats(&PUT, &PUT_DURATION_NANOS, &PUT_MAX_NANOS),
        head:   latency_stats(&HEAD, &HEAD_DURATION_NANOS, &HEAD_MAX_NANOS),
        delete: latency_stats(&DELETE, &DELETE_DURATION_NANOS, &DELETE_MAX_NANOS),
    }
}

fn latency_stats(count: &AtomicU64, sum_nanos: &AtomicU64, max_nanos: &AtomicU64) -> LatencyStats {
    let n = count.load(Ordering::Relaxed);
    let sum = sum_nanos.load(Ordering::Relaxed);
    let max = max_nanos.load(Ordering::Relaxed);
    LatencyStats {
        count: n,
        avg_ms: if n == 0 { 0.0 } else { (sum as f64 / n as f64) / 1_000_000.0 },
        max_ms: (max as f64) / 1_000_000.0,
    }
}

pub fn reset() {
    GET.store(0, Ordering::Relaxed);
    PUT.store(0, Ordering::Relaxed);
    HEAD.store(0, Ordering::Relaxed);
    DELETE.store(0, Ordering::Relaxed);
    LIST.store(0, Ordering::Relaxed);
    DELETE_OBJECTS.store(0, Ordering::Relaxed);
    COPY.store(0, Ordering::Relaxed);
    MULTIPART.store(0, Ordering::Relaxed);

    GET_DURATION_NANOS.store(0, Ordering::Relaxed);
    PUT_DURATION_NANOS.store(0, Ordering::Relaxed);
    HEAD_DURATION_NANOS.store(0, Ordering::Relaxed);
    DELETE_DURATION_NANOS.store(0, Ordering::Relaxed);

    GET_MAX_NANOS.store(0, Ordering::Relaxed);
    PUT_MAX_NANOS.store(0, Ordering::Relaxed);
    HEAD_MAX_NANOS.store(0, Ordering::Relaxed);
    DELETE_MAX_NANOS.store(0, Ordering::Relaxed);
}

#[derive(serde::Serialize)]
pub struct OpCounts {
    pub get:            u64,
    pub put:            u64,
    pub head:           u64,
    pub delete:         u64,
    pub list:           u64,
    pub delete_objects: u64,
    pub copy:           u64,
    pub multipart:      u64,
}

impl OpCounts {
    /// Estimated monthly USD cost using AWS S3 pricing (us-east-1, 2025).
    /// GET/HEAD: $0.0004 / 1000 requests
    /// PUT/COPY/POST/LIST: $0.005 / 1000 requests
    pub fn estimated_cost_usd(&self) -> f64 {
        let reads  = (self.get + self.head) as f64;
        let writes = (self.put + self.copy + self.list + self.delete_objects + self.multipart) as f64;
        reads  * 0.0004 / 1000.0
            + writes * 0.005  / 1000.0
    }
}

#[derive(serde::Serialize)]
pub struct LatencyStats {
    pub count:  u64,
    pub avg_ms: f64,
    pub max_ms: f64,
}

#[derive(serde::Serialize)]
pub struct OpLatencies {
    pub get:    LatencyStats,
    pub put:    LatencyStats,
    pub head:   LatencyStats,
    pub delete: LatencyStats,
}
