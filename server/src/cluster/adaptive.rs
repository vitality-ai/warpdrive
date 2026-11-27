//! Compression that follows the bottleneck.
//!
//! zstd trades CPU for bytes: it costs this node CPU and saves network and
//! disk on every node a blob goes to. Which side of that trade wins depends
//! on what's short right now. Measured on GCP (2026-11-27, 5 x
//! n2-standard-2, RS(3,2), 64 KB JSON-lines objects): with node 0 at 97%
//! CPU, zstd cut network bytes per PUT by three quarters and PUT throughput
//! by 17%. So compression isn't a fixed setting. This module decides per
//! blob, from what the node measures:
//!
//! - this node's CPU busy fraction and NIC send rate, sampled every 500 ms;
//! - how fast zstd compresses here, and how well recent blobs compressed.
//!
//! The cost function is time per blob:
//!
//! ```text
//! raw  = moved / (link * spare_network)
//! zstd = size * compress_secs_per_byte / spare_cpu + moved * ratio / (link * spare_network)
//! ```
//!
//! `moved` is the bytes the blob puts on the network: its `k+m` shards are
//! `(k+m)/k` times its size. `spare_cpu` and `spare_network` are the free
//! fractions of what this node has (floored at 5%), so a busy resource makes
//! its term expensive. The blob is compressed when `zstd < raw`.
//!
//! Disk space counts for a little: the bytes zstd saves on disk are
//! credited at `epsilon` times the time it would take to send them
//! (`WARPDRIVE_COMPRESS_DISK_EPSILON`, default 0.05). It tips close calls
//! toward storing less, without outweighing a busy CPU or network.
//!
//! ```text
//! zstd -= epsilon * moved * (1 - ratio) / link
//! ```
//!
//! Reads count too: a GET fetches a blob's shards to the node serving it,
//! so a compressed blob saves network on every read, not just once.
//! Measured with every PUT compressed (64 KB to 8 MB text): PUT throughput
//! fell up to 17% while GET throughput rose up to 36% from 1 MB up. The
//! policy counts reads per write on this node and credits the bytes later
//! reads won't move:
//!
//! ```text
//! zstd -= reads_per_write * moved * (1 - ratio) / (link * spare_network)
//! ```
//!
//! While the answer is "raw", one blob in `SAMPLE_EVERY` is compressed
//! anyway, so the ratio and speed stay current as data and load change.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Compress one blob in this many even when the cost says not to.
const SAMPLE_EVERY: u64 = 32;
/// Weight of a new observation in the moving averages.
const ALPHA: f64 = 0.05;
/// The least spare fraction a resource is assumed to have.
const FLOOR: f64 = 0.05;
/// Default weight of disk space saved, relative to sending those bytes.
const DISK_EPSILON: f64 = 0.05;

fn load_f64(a: &AtomicU64) -> f64 {
    f64::from_bits(a.load(Ordering::Relaxed))
}

fn store_f64(a: &AtomicU64, v: f64) {
    a.store(v.to_bits(), Ordering::Relaxed)
}

fn ewma(a: &AtomicU64, v: f64) {
    let old = load_f64(a);
    store_f64(a, if old == 0.0 { v } else { old + ALPHA * (v - old) });
}

/// This node's load, sampled in the background from `/proc` (Linux). Where
/// there's no `/proc`, both read as idle.
pub struct NodeLoad {
    cpu_busy: AtomicU64,
    tx_bytes_per_sec: AtomicU64,
    /// The highest send rate seen: a floor on what the link can do.
    peak_tx_bytes_per_sec: AtomicU64,
    /// Free fraction of the filesystem this node stores in (its working
    /// directory), 1.0 where it can't be read.
    disk_free: AtomicU64,
}

impl NodeLoad {
    pub fn start() -> Arc<Self> {
        let load = Arc::new(Self {
            cpu_busy: AtomicU64::new(0),
            tx_bytes_per_sec: AtomicU64::new(0),
            peak_tx_bytes_per_sec: AtomicU64::new(0),
            disk_free: AtomicU64::new(1f64.to_bits()),
        });
        let weak = Arc::downgrade(&load);
        std::thread::Builder::new()
            .name("wd-node-load".into())
            .spawn(move || {
                let (mut cpu, mut tx, mut at) = (read_cpu(), read_tx(), Instant::now());
                loop {
                    std::thread::sleep(Duration::from_millis(500));
                    let Some(load) = weak.upgrade() else { return };
                    let (c, t, now) = (read_cpu(), read_tx(), Instant::now());
                    if let (Some((total0, idle0)), Some((total1, idle1))) = (cpu, c) {
                        let dt = total1.saturating_sub(total0);
                        if dt > 0 {
                            store_f64(&load.cpu_busy, 1.0 - idle1.saturating_sub(idle0) as f64 / dt as f64);
                        }
                    }
                    if let (Some(t0), Some(t1)) = (tx, t) {
                        let rate = t1.saturating_sub(t0) as f64 / now.duration_since(at).as_secs_f64();
                        store_f64(&load.tx_bytes_per_sec, rate);
                        if rate > load_f64(&load.peak_tx_bytes_per_sec) {
                            store_f64(&load.peak_tx_bytes_per_sec, rate);
                        }
                    }
                    if let Some(free) = disk_free(".") {
                        store_f64(&load.disk_free, free);
                    }
                    (cpu, tx, at) = (c, t, now);
                }
            })
            .expect("starting the node load sampler");
        load
    }

    pub fn cpu_busy(&self) -> f64 {
        load_f64(&self.cpu_busy)
    }

    pub fn tx_bytes_per_sec(&self) -> f64 {
        load_f64(&self.tx_bytes_per_sec)
    }

    pub fn peak_tx_bytes_per_sec(&self) -> f64 {
        load_f64(&self.peak_tx_bytes_per_sec)
    }

    pub fn disk_free(&self) -> f64 {
        load_f64(&self.disk_free)
    }
}

/// Free fraction of the filesystem holding `path`.
fn disk_free(path: &str) -> Option<f64> {
    let c = std::ffi::CString::new(path).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid C string and `st` a valid out-pointer.
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 || st.f_blocks == 0 {
        return None;
    }
    Some(st.f_bavail as f64 / st.f_blocks as f64)
}

/// `(total, idle)` jiffies over all CPUs.
fn read_cpu() -> Option<(u64, u64)> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    let line = stat.lines().next()?.strip_prefix("cpu ")?;
    let v: Vec<u64> = line.split_whitespace().filter_map(|x| x.parse().ok()).collect();
    let idle = v.get(3)? + v.get(4).copied().unwrap_or(0);
    Some((v.iter().sum(), idle))
}

/// Bytes sent on every interface but loopback.
fn read_tx() -> Option<u64> {
    let dev = std::fs::read_to_string("/proc/net/dev").ok()?;
    Some(
        dev.lines()
            .skip(2)
            .filter_map(|l| {
                let (name, rest) = l.split_once(':')?;
                (name.trim() != "lo").then(|| rest.split_whitespace().nth(8)?.parse::<u64>().ok()).flatten()
            })
            .sum(),
    )
}

/// The `auto` policy: compress a blob when the cost function says it's
/// cheaper right now.
pub struct Adaptive {
    load: Arc<NodeLoad>,
    /// The link's send capacity in bytes per second, at least: the larger of
    /// `WARPDRIVE_NET_GBPS` (default 10, an n2-standard-2's egress cap) and
    /// the highest send rate this node has seen.
    net_floor: f64,
    /// Weight of disk space saved while the disk is empty
    /// (`WARPDRIVE_COMPRESS_DISK_EPSILON`). It grows as the disk fills
    /// (`disk_weight`).
    disk_epsilon: f64,
    /// Compressed size over raw size, recent blobs.
    ratio: AtomicU64,
    /// Seconds zstd takes per input byte here, recent blobs.
    secs_per_byte: AtomicU64,
    seen: AtomicU64,
    compressed: AtomicU64,
    raw: AtomicU64,
    reads: AtomicU64,
}

/// Reads per write are capped here, so a read-heavy burst can't override
/// a busy CPU on its own.
const MAX_READS_PER_WRITE: f64 = 4.0;

/// The disk weight for a free fraction: `epsilon` on an empty disk, rising
/// as free space runs out (`epsilon / free`, free floored at 2%). On a disk
/// 98% full, saving space outweighs a busy CPU.
pub fn disk_weight(epsilon: f64, free: f64) -> f64 {
    epsilon / free.clamp(0.02, 1.0)
}

impl Adaptive {
    pub fn new(load: Arc<NodeLoad>) -> Self {
        let gbps = std::env::var("WARPDRIVE_NET_GBPS").ok().and_then(|v| v.trim().parse::<f64>().ok()).filter(|g| *g > 0.0).unwrap_or(10.0);
        let disk_epsilon = std::env::var("WARPDRIVE_COMPRESS_DISK_EPSILON").ok().and_then(|v| v.trim().parse::<f64>().ok()).filter(|e| *e >= 0.0).unwrap_or(DISK_EPSILON);
        Self {
            load,
            net_floor: gbps * 1e9 / 8.0,
            disk_epsilon,
            ratio: AtomicU64::new(0),
            secs_per_byte: AtomicU64::new(0),
            seen: AtomicU64::new(0),
            compressed: AtomicU64::new(0),
            raw: AtomicU64::new(0),
            reads: AtomicU64::new(0),
        }
    }

    fn reads_per_write(&self) -> f64 {
        let writes = self.compressed.load(Ordering::Relaxed) + self.raw.load(Ordering::Relaxed);
        if writes == 0 {
            return 0.0;
        }
        (self.reads.load(Ordering::Relaxed) as f64 / writes as f64).min(MAX_READS_PER_WRITE)
    }

    fn link(&self) -> f64 {
        self.net_floor.max(self.load.peak_tx_bytes_per_sec())
    }

    /// Whether compressing a blob of `size` bytes is cheaper right now.
    /// Always yes until the first measurements are in.
    fn worth_it(&self, size: usize, wire_factor: f64) -> bool {
        let (ratio, spb) = (load_f64(&self.ratio), load_f64(&self.secs_per_byte));
        if ratio == 0.0 || spb == 0.0 {
            return true;
        }
        let link = self.link();
        let eps = disk_weight(self.disk_epsilon, self.load.disk_free());
        let net_busy = self.load.tx_bytes_per_sec() / link;
        let moved = size as f64 * wire_factor;
        let now = cost(moved, size as f64, ratio, spb, self.load.cpu_busy(), net_busy, link, eps);
        now - read_credit(moved, ratio, self.reads_per_write(), net_busy, link) < 0.0
    }
}

impl super::blob_io::CompressionPolicy for Adaptive {
    fn name(&self) -> String {
        "auto: when this node's CPU, network and disk say it pays".into()
    }

    /// The cost function, or one blob in `SAMPLE_EVERY` regardless, so the
    /// ratio and speed stay current.
    fn try_compress(&self, size: usize, wire_factor: f64) -> bool {
        self.seen.fetch_add(1, Ordering::Relaxed) % SAMPLE_EVERY == 0 || self.worth_it(size, wire_factor)
    }

    fn observe(&self, size: usize, out: usize, secs: f64) {
        ewma(&self.ratio, out as f64 / size as f64);
        ewma(&self.secs_per_byte, secs / size as f64);
    }

    fn read(&self) {
        self.reads.fetch_add(1, Ordering::Relaxed);
    }

    fn stored(&self, compressed: bool) {
        let c = if compressed { &self.compressed } else { &self.raw };
        c.fetch_add(1, Ordering::Relaxed);
    }

    fn view(&self) -> Option<serde_json::Value> {
        let spb = load_f64(&self.secs_per_byte);
        let link = self.link();
        Some(serde_json::json!({
            "cpu_busy": self.load.cpu_busy(),
            "net_busy": self.load.tx_bytes_per_sec() / link,
            "link_mb_per_s": link / 1e6,
            "disk_free": self.load.disk_free(),
            "disk_weight": disk_weight(self.disk_epsilon, self.load.disk_free()),
            "ratio": load_f64(&self.ratio),
            "compress_mb_per_s": if spb > 0.0 { 1e-6 / spb } else { 0.0 },
            "reads_per_write": self.reads_per_write(),
            "compressed": self.compressed.load(Ordering::Relaxed),
            "raw": self.raw.load(Ordering::Relaxed),
        }))
    }
}

/// zstd's time minus raw's for one blob, in seconds. Negative means
/// compress. `moved` is the bytes the blob puts on the network, `size` the
/// bytes zstd reads, `cpu_busy` and `net_busy` fractions in 0..1, and
/// `link` the node's send capacity in bytes per second, and `epsilon` the
/// weight of the disk space saved (every shard sent is stored).
#[allow(clippy::too_many_arguments)]
pub fn cost(moved: f64, size: f64, ratio: f64, secs_per_byte: f64, cpu_busy: f64, net_busy: f64, link: f64, epsilon: f64) -> f64 {
    let spare_cpu = (1.0 - cpu_busy).max(FLOOR);
    let spare_net = link * (1.0 - net_busy).max(FLOOR);
    let raw = moved / spare_net;
    let disk_credit = epsilon * moved * (1.0 - ratio).max(0.0) / link;
    let zstd = size * secs_per_byte / spare_cpu + moved * ratio / spare_net - disk_credit;
    zstd - raw
}

/// Seconds of network that later reads of a compressed blob won't spend.
pub fn read_credit(moved: f64, ratio: f64, reads_per_write: f64, net_busy: f64, link: f64) -> f64 {
    reads_per_write * moved * (1.0 - ratio).max(0.0) / (link * (1.0 - net_busy).max(FLOOR))
}

#[cfg(test)]
mod tests {
    // Text compresses to 30% at 400 MB/s (2.5 ns/byte). RS(3,2) puts 5/3
    // of a blob on the wire. A 10 Gbit/s link.
    const RATIO: f64 = 0.3;
    const SPB: f64 = 2.5e-9;
    const LINK: f64 = 1.25e9;

    fn cost(size: f64, ratio: f64, spb: f64, cpu: f64, net: f64) -> f64 {
        super::cost(size * 5.0 / 3.0, size, ratio, spb, cpu, net, LINK, super::DISK_EPSILON)
    }

    #[test]
    fn a_filling_disk_raises_the_disk_weight() {
        assert_eq!(super::disk_weight(0.05, 1.0), 0.05);
        assert!((super::disk_weight(0.05, 0.1) - 0.5).abs() < 1e-12);
        assert_eq!(super::disk_weight(0.05, 0.0), 2.5, "floored at 2% free");
        let nearly_full = super::disk_weight(super::DISK_EPSILON, 0.02);
        assert!(super::cost(65536.0 * 5.0 / 3.0, 65536.0, RATIO, SPB, 0.10, 0.10, LINK, nearly_full) < 0.0, "an idle node with a nearly full disk compresses");
    }

    #[test]
    fn reads_credit_compression_but_not_past_a_busy_cpu() {
        let size = 1048576.0;
        let moved = size * 5.0 / 3.0;
        // CPU 60% busy, network 60% busy: raw on write cost alone.
        let write = super::cost(moved, size, RATIO, SPB, 0.60, 0.60, LINK, super::DISK_EPSILON);
        assert!(write > 0.0);
        assert!(write - super::read_credit(moved, RATIO, 2.0, 0.60, LINK) < 0.0, "two reads per write tip it to zstd");
        let busy = super::cost(moved, size, RATIO, SPB, 0.97, 0.10, LINK, super::DISK_EPSILON);
        assert!(busy - super::read_credit(moved, RATIO, super::MAX_READS_PER_WRITE, 0.10, LINK) > 0.0, "a saturated CPU still says raw");
    }

    #[test]
    fn a_busy_cpu_turns_compression_off() {
        assert!(cost(65536.0, RATIO, SPB, 0.97, 0.10) > 0.0, "node 0 at 97% CPU, network idle: raw");
    }

    #[test]
    fn a_busy_network_with_spare_cpu_turns_it_on() {
        assert!(cost(65536.0, RATIO, SPB, 0.30, 0.90) < 0.0, "network 90% busy, CPU 30%: compress");
    }

    #[test]
    fn data_that_does_not_shrink_is_never_worth_it() {
        assert!(cost(65536.0, 0.99, SPB, 0.10, 0.90) > 0.0, "random bytes: the CPU buys nothing");
    }

    #[test]
    fn an_idle_node_sends_raw() {
        assert!(cost(65536.0, RATIO, SPB, 0.10, 0.10) > 0.0, "nothing is short: compression only adds time");
    }

    #[test]
    fn disk_space_tips_a_close_call() {
        // CPU 46.5% busy, network 80% busy, text: about even on time alone.
        let close = |eps| super::cost(65536.0 * 5.0 / 3.0, 65536.0, RATIO, SPB, 0.465, 0.80, LINK, eps);
        assert!(close(0.0) > 0.0, "without disk, raw by a hair");
        assert!(close(super::DISK_EPSILON) < 0.0, "the disk credit tips it to zstd");
        assert!(super::cost(65536.0 * 5.0 / 3.0, 65536.0, RATIO, SPB, 0.97, 0.10, LINK, super::DISK_EPSILON) > 0.0, "but never past a busy CPU");
    }
}
