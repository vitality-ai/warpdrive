use std::sync::atomic::{AtomicU64, Ordering};

pub static GET: AtomicU64 = AtomicU64::new(0);
pub static PUT: AtomicU64 = AtomicU64::new(0);
pub static HEAD: AtomicU64 = AtomicU64::new(0);
pub static DELETE: AtomicU64 = AtomicU64::new(0);
pub static LIST: AtomicU64 = AtomicU64::new(0);
pub static DELETE_OBJECTS: AtomicU64 = AtomicU64::new(0);
pub static COPY: AtomicU64 = AtomicU64::new(0);
pub static MULTIPART: AtomicU64 = AtomicU64::new(0);

/// Increment a counter. Compiled to nothing without the `op-counters` feature.
#[macro_export]
macro_rules! count {
    ($counter:expr) => {
        #[cfg(feature = "op-counters")]
        $counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    };
}

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

pub fn reset() {
    GET.store(0, Ordering::Relaxed);
    PUT.store(0, Ordering::Relaxed);
    HEAD.store(0, Ordering::Relaxed);
    DELETE.store(0, Ordering::Relaxed);
    LIST.store(0, Ordering::Relaxed);
    DELETE_OBJECTS.store(0, Ordering::Relaxed);
    COPY.store(0, Ordering::Relaxed);
    MULTIPART.store(0, Ordering::Relaxed);
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
