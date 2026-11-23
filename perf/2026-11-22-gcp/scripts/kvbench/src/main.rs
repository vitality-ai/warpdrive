//! kvbench <bitcask|rocksdb> <dir> <keys> <threads> <get_secs> [cache_mb]
//!
//! The same workload through WarpMetaStore's BitcaskEngine and through RocksDB:
//! 1. Load `keys` new rows (about 900 bytes, the size of a real object row) from
//!    `threads` writers, each write durable before it returns (Bitcask: group
//!    commit; RocksDB: WAL with sync). Every 10% of the keys: throughput, p50,
//!    p99 and RSS.
//! 2. Random GETs for `get_secs` from `threads` readers: throughput, p50, p99.
//! 3. 200 prefix scans of 1,000 rows each.
//! 4. Close, reopen, time to open and RSS after reopen.
//!
//! RocksDB's memory is bounded by its block cache (`cache_mb`, default 256)
//! plus memtables (2 x 64 MB). Prints CSV lines prefixed with the phase.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use warp_meta_store::{BitcaskEngine, Engine};

trait Kv: Send + Sync {
    fn put(&self, k: String, v: String);
    fn get(&self, k: &str) -> Option<String>;
    fn scan(&self, prefix: &str) -> usize;
}

struct Bc(BitcaskEngine<String>);
impl Kv for Bc {
    fn put(&self, k: String, v: String) {
        self.0.put(k, v).unwrap()
    }
    fn get(&self, k: &str) -> Option<String> {
        self.0.get(k)
    }
    fn scan(&self, prefix: &str) -> usize {
        self.0.scan_prefix(prefix, |_, v| Some(v.len())).len()
    }
}

struct Rk(rocksdb::DB, rocksdb::WriteOptions);
impl Kv for Rk {
    fn put(&self, k: String, v: String) {
        self.0.put_opt(k.as_bytes(), v.as_bytes(), &self.1).unwrap()
    }
    fn get(&self, k: &str) -> Option<String> {
        self.0.get(k.as_bytes()).unwrap().map(|b| String::from_utf8(b).unwrap())
    }
    fn scan(&self, prefix: &str) -> usize {
        let mut n = 0;
        let it = self.0.iterator(rocksdb::IteratorMode::From(prefix.as_bytes(), rocksdb::Direction::Forward));
        for kv in it {
            let (k, _v) = kv.unwrap();
            if !k.starts_with(prefix.as_bytes()) {
                break;
            }
            n += 1;
        }
        n
    }
}

fn open(engine: &str, dir: &str, cache_mb: usize) -> Box<dyn Kv> {
    match engine {
        "bitcask" => Box::new(Bc(BitcaskEngine::open(format!("{dir}/meta.log"), "bench").unwrap())),
        "rocksdb" => {
            let mut bb = rocksdb::BlockBasedOptions::default();
            let cache = rocksdb::Cache::new_lru_cache(cache_mb << 20);
            bb.set_block_cache(&cache);
            bb.set_bloom_filter(10.0, false);
            bb.set_cache_index_and_filter_blocks(true);
            bb.set_pin_l0_filter_and_index_blocks_in_cache(true);
            // Read-side options under test (KVB_* env): a hash index inside data blocks.
            if std::env::var("KVB_HASH_INDEX").is_ok() {
                bb.set_data_block_index_type(rocksdb::DataBlockIndexType::BinaryAndHash);
                bb.set_data_block_hash_ratio(0.75);
            }
            let mut o = rocksdb::Options::default();
            o.create_if_missing(true);
            o.set_block_based_table_factory(&bb);
            o.set_max_background_jobs(4);
            o.set_write_buffer_size(64 << 20);
            o.set_max_write_buffer_number(2);
            o.set_compression_type(if std::env::var("KVB_NO_COMPRESSION").is_ok() { rocksdb::DBCompressionType::None } else { rocksdb::DBCompressionType::Lz4 });
            if let Some(mb) = std::env::var("KVB_ROW_CACHE_MB").ok().and_then(|v| v.parse::<usize>().ok()) {
                o.set_row_cache(&rocksdb::Cache::new_lru_cache(mb << 20));
            }
            o.set_max_open_files(512);
            let mut w = rocksdb::WriteOptions::default();
            w.set_sync(true);
            Box::new(Rk(rocksdb::DB::open(&o, format!("{dir}/rocks")).unwrap(), w))
        }
        e => panic!("engine {e}: bitcask or rocksdb"),
    }
}

fn rss_mb() -> f64 {
    if let Ok(s) = std::fs::read_to_string("/proc/self/statm") {
        let pages: f64 = s.split_whitespace().nth(1).unwrap().parse().unwrap();
        return pages * 4096.0 / 1048576.0;
    }
    let out = std::process::Command::new("ps").args(["-o", "rss=", "-p", &std::process::id().to_string()]).output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse::<f64>().unwrap_or(0.0) / 1024.0
}

/// Writer w's i-th key: realistic object-row key, sorted by writer then index.
fn key(w: usize, i: usize) -> String {
    format!("ver/5:admin/5:bench/w{w:03}-{i:09}\u{0}18ea8faeaa377162")
}

/// About 900 bytes, shaped like a real version row (JSON with a few unique fields).
fn value(w: usize, i: usize) -> String {
    let v = format!(
        r#"{{"Live":{{"seq":{s},"value":{{"Version":{{"order":{s},"version_id":"","is_delete_marker":false,"meta":{{"chunks":[{{"offset":{o},"size":4096}}],"properties":{{}},"etag":"\"{e:032x}\"","size":4096,"content_type":"application/octet-stream","last_modified":"2026-11-23T03:43:01.000Z","user_metadata":{{}},"cache_control":null,"expires":null,"content_encoding":null,"version_id":null,"is_delete_marker":false,"checksum_algorithm":"CRC32C","checksum_value":"{c:08x}==","checksum_type":null}},"tags":[],"parts_manifest":null,"blobs":[{{"user_id":"admin","bucket":"bench","base":{o},"placement":{{"shard_peers":["http://10.128.0.49:9710","http://10.128.0.47:9710","http://10.128.0.48:9710"],"k":2,"m":1,"wire_key":"{o:016x}:{e:016x}-{w}","len":4096,"inline":null}}}}]}}}}}}}}"#,
        s = 1795405381803423142u64 + (w * 1_000_000 + i) as u64,
        o = (i as u64) << 12,
        e = (w as u64) << 40 | i as u64,
        c = (i * 2654435761) as u32,
    );
    v
}

fn pct(v: &mut [u32], p: f64) -> u32 {
    if v.is_empty() {
        return 0;
    }
    v.sort_unstable();
    v[((v.len() as f64 * p) as usize).min(v.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::parking_lot_free::*;
    #[test]
    fn buckets_are_monotonic_and_close() {
        let mut last = 0;
        for us in 1..2_000_000u32 {
            let b = bucket(us);
            assert!(b >= last);
            last = b;
            let lo = lower(b);
            assert!(lo <= us as u64 && (us as u64 - lo) * 16 <= us as u64 + 16, "{us} -> {b} lower {lo}");
        }
    }
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (engine, dir) = (a[1].clone(), a[2].clone());
    let keys: usize = a[3].parse().unwrap();
    let threads: usize = a[4].parse().unwrap();
    let get_secs: u64 = a[5].parse().unwrap();
    let cache_mb: usize = a.get(6).map(|s| s.parse().unwrap()).unwrap_or(256);
    std::fs::create_dir_all(&dir).unwrap();
    let per = keys / threads;
    let kv: Arc<dyn Kv> = Arc::from(open(&engine, &dir, cache_mb));
    let opts: Vec<String> = std::env::vars().filter(|(k, _)| k.starts_with("KVB_")).map(|(k, v)| format!("{k}={v}")).collect();
    println!("start,{engine},keys={keys},threads={threads},cache_mb={cache_mb},value_bytes={},rss_mb={:.0},opts={}", value(0, 0).len(), rss_mb(), opts.join(" "));

    // 1. Load, reported every 10% of the keys.
    let done = Arc::new(AtomicU64::new(0));
    let lat: Vec<Arc<Lats>> = (0..threads).map(|_| Arc::new(Lats::default())).collect();
    println!("load,engine,keys_done,secs,puts_per_s,p50_us,p99_us,p999_us,rss_mb");
    let t0 = Instant::now();
    let hs: Vec<_> = (0..threads)
        .map(|w| {
            let (kv, done, lat) = (kv.clone(), done.clone(), lat[w].clone());
            std::thread::spawn(move || {
                for i in 0..per {
                    let (k, v) = (key(w, i), value(w, i));
                    let t = Instant::now();
                    kv.put(k, v);
                    lat.push(t.elapsed().as_micros() as u32);
                    done.fetch_add(1, Ordering::Relaxed);
                }
            })
        })
        .collect();
    let step = (per * threads / 10).max(1) as u64;
    let (mut next, mut last_t, mut last_n) = (step, t0, 0u64);
    loop {
        std::thread::sleep(Duration::from_millis(200));
        let n = done.load(Ordering::Relaxed);
        let finished = hs.iter().all(|h| h.is_finished());
        if n >= next || finished {
            let l = merge(lat.iter().map(|x| x.take()));
            let dt = last_t.elapsed().as_secs_f64();
            println!(
                "load,{engine},{n},{:.1},{:.0},{},{},{},{:.0}",
                t0.elapsed().as_secs_f64(),
                (n - last_n) as f64 / dt,
                hpct(&l, 0.5),
                hpct(&l, 0.99),
                hpct(&l, 0.999),
                rss_mb()
            );
            (last_t, last_n) = (Instant::now(), n);
            while next <= n {
                next += step;
            }
        }
        if finished {
            break;
        }
    }
    hs.into_iter().for_each(|h| h.join().unwrap());
    let total = done.load(Ordering::Relaxed) as usize;

    // 2. Random GETs.
    let stop = Arc::new(AtomicBool::new(false));
    let hs: Vec<_> = (0..threads)
        .map(|w| {
            let (kv, stop) = (kv.clone(), stop.clone());
            std::thread::spawn(move || {
                let (mut x, l, mut miss) = (0x9E3779B97F4A7C15u64 ^ (w as u64 + 1), Lats::default(), 0u64);
                while !stop.load(Ordering::Relaxed) {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    let (ww, i) = ((x % threads as u64) as usize, ((x >> 20) % per as u64) as usize);
                    let t = Instant::now();
                    if kv.get(&key(ww, i)).is_none() {
                        miss += 1;
                    }
                    l.push(t.elapsed().as_micros() as u32);
                }
                (l.take(), miss)
            })
        })
        .collect();
    std::thread::sleep(Duration::from_secs(get_secs));
    stop.store(true, Ordering::Relaxed);
    let (mut hs2, mut miss) = (Vec::new(), 0);
    for h in hs {
        let (x, m) = h.join().unwrap();
        hs2.push(x);
        miss += m;
    }
    let l = merge(hs2.into_iter());
    println!("get,engine,gets_per_s,p50_us,p99_us,p999_us,missing,rss_mb");
    println!("get,{engine},{:.0},{},{},{},{miss},{:.0}", count(&l) as f64 / get_secs as f64, hpct(&l, 0.5), hpct(&l, 0.99), hpct(&l, 0.999), rss_mb());

    // 3. Prefix scans of 1,000 rows (writer w, index block of 1,000).
    let mut sl = Vec::new();
    let mut rows = 0;
    for s in 0..200usize {
        let (w, blk) = (s % threads, (s * 7919) % (per / 1000).max(1));
        let prefix = format!("ver/5:admin/5:bench/w{w:03}-{:06}", blk);
        let t = Instant::now();
        rows += kv.scan(&prefix);
        sl.push(t.elapsed().as_micros() as u32);
    }
    println!("scan,engine,scans,rows_per_scan,p50_us,p99_us");
    println!("scan,{engine},200,{},{},{}", rows / 200, pct(&mut sl, 0.5), pct(&mut sl, 0.99));

    // 4. Reopen.
    drop(kv);
    let t = Instant::now();
    let kv = open(&engine, &dir, cache_mb);
    let open_s = t.elapsed().as_secs_f64();
    let ok = kv.get(&key(threads / 2, per / 2)).is_some();
    println!("reopen,engine,open_secs,found_sample,rss_mb,keys");
    println!("reopen,{engine},{open_s:.1},{ok},{:.0},{total}", rss_mb());
}

/// Fixed-size latency histograms, so the benchmark's own memory stays
/// constant: 16 linear sub-buckets per power of two of microseconds (about 6%
/// resolution).
mod parking_lot_free {
    use std::sync::atomic::{AtomicU64, Ordering};
    pub const N: usize = 40 * 16;
    pub fn bucket(us: u32) -> usize {
        let v = us.max(1) as u64;
        let e = 63 - v.leading_zeros() as usize;
        if e < 4 { return v as usize; }
        (e - 3) * 16 + ((v >> (e - 4)) as usize & 15)
    }
    pub fn lower(b: usize) -> u64 {
        if b < 16 { return b as u64; }
        let (e, m) = (b / 16 + 3, (b % 16) as u64);
        (16 + m) << (e - 4)
    }
    pub struct Lats(Vec<AtomicU64>);
    impl Default for Lats {
        fn default() -> Self { Lats((0..N).map(|_| AtomicU64::new(0)).collect()) }
    }
    impl Lats {
        pub fn push(&self, us: u32) { self.0[bucket(us).min(N - 1)].fetch_add(1, Ordering::Relaxed); }
        /// Drains the counts.
        pub fn take(&self) -> Vec<u64> { self.0.iter().map(|c| c.swap(0, Ordering::Relaxed)).collect() }
    }
    pub fn merge(hs: impl Iterator<Item = Vec<u64>>) -> Vec<u64> {
        let mut out = vec![0u64; N];
        for h in hs { for (o, c) in out.iter_mut().zip(h) { *o += c; } }
        out
    }
    pub fn pct(h: &[u64], p: f64) -> u64 {
        let total: u64 = h.iter().sum();
        if total == 0 { return 0; }
        let want = ((total as f64 * p).ceil() as u64).max(1);
        let mut seen = 0;
        for (b, c) in h.iter().enumerate() { seen += c; if seen >= want { return lower(b); } }
        lower(N - 1)
    }
    pub fn count(h: &[u64]) -> u64 { h.iter().sum() }
}
use parking_lot_free::{merge, pct as hpct, count, Lats};
