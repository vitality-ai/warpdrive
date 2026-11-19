//! segbench: RFC 6 storage-layer experiment.
//!
//! Objects (shards) are appended to fixed-size segment files with group-commit
//! fsync. Each object gets a lifetime (hot: short, cold: long), measured in
//! bytes written, so the live set reaches a steady state. A compactor reclaims
//! sealed segments with the configured policy, layout and copy method, while
//! foreground writers and readers run at full speed.
//!
//! Prints one JSON summary line; with --timeline writes a per-second CSV.

use std::collections::{BTreeMap, BinaryHeap, HashMap};
use std::cmp::Reverse;
use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
#[cfg(target_os = "linux")]
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
struct Cfg {
    dir: PathBuf,
    segment_mb: u64,
    umax: f64,
    policy: String,  // greedy | costbenefit
    layout: String,  // A (survivors into the active segment) | C (separate survivor stream)
    copy: String,    // rw | cfr (copy_file_range)
    rate_mbps: f64,  // compaction rate limit, 0 = unlimited
    live_gb: f64,    // target steady-state live bytes
    churn_x: f64,    // measured churn, in multiples of live_gb
    warm_x: f64,     // warm-up churn before measuring
    writers: usize,
    readers: usize,
    compact: bool,
    hot_frac: f64,
    seed: u64,
    timeline: Option<PathBuf>,
}

fn parse() -> Cfg {
    let mut c = Cfg {
        dir: PathBuf::from("/mnt/nvme/segbench"), segment_mb: 256, umax: 0.5, policy: "costbenefit".into(),
        layout: "C".into(), copy: "cfr".into(), rate_mbps: 50.0, live_gb: 6.0, churn_x: 4.0, warm_x: 2.0,
        writers: 8, readers: 4, compact: true, hot_frac: 0.9, seed: 1, timeline: None,
    };
    let a: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < a.len() {
        let v = a.get(i + 1).cloned().unwrap_or_default();
        match a[i].as_str() {
            "--dir" => c.dir = v.into(),
            "--segment-mb" => c.segment_mb = v.parse().unwrap(),
            "--umax" => c.umax = v.parse().unwrap(),
            "--policy" => c.policy = v,
            "--layout" => c.layout = v,
            "--copy" => c.copy = v,
            "--rate-mbps" => c.rate_mbps = v.parse().unwrap(),
            "--live-gb" => c.live_gb = v.parse().unwrap(),
            "--churn-x" => c.churn_x = v.parse().unwrap(),
            "--warm-x" => c.warm_x = v.parse().unwrap(),
            "--writers" => c.writers = v.parse().unwrap(),
            "--readers" => c.readers = v.parse().unwrap(),
            "--compact" => c.compact = v == "1",
            "--hot-frac" => c.hot_frac = v.parse().unwrap(),
            "--seed" => c.seed = v.parse().unwrap(),
            "--timeline" => c.timeline = Some(v.into()),
            x => panic!("unknown arg {x}"),
        }
        i += 2;
    }
    c
}

// ------------------------------------------------------------------ rng
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 { self.0 ^= self.0 << 13; self.0 ^= self.0 >> 7; self.0 ^= self.0 << 17; self.0 }
    fn f(&mut self) -> f64 { (self.next() >> 11) as f64 / (1u64 << 53) as f64 }
    fn exp(&mut self, mean: f64) -> f64 { -mean * (1.0 - self.f()).ln() }
}

// ------------------------------------------------------------------ state
struct Obj { seg: u64, off: u64, len: u64, pos: usize }
struct Seg { file: Arc<File>, len: u64, live: u64, sealed: bool, survivor: bool, newest: Instant }

struct Inner {
    objs: HashMap<u64, Obj>,
    ids: Vec<u64>,
    segs: BTreeMap<u64, Seg>,
    active: u64,
    active_surv: u64,
    next_seg: u64,
    deaths: BinaryHeap<Reverse<(u64, u64)>>,
    dirty: Vec<u64>,
    disk_bytes: u64,
    live_bytes: u64,
}

struct Shared {
    cfg: Cfg,
    seg_size: u64,
    st: Mutex<Inner>,
    clock: AtomicU64,          // user bytes written (the lifetime clock)
    fg_bytes: AtomicU64,
    comp_bytes: AtomicU64,
    fg_ops: AtomicU64,
    rd_ops: AtomicU64,
    compacting: AtomicBool,
    comp_busy_us: AtomicU64,
    segs_compacted: AtomicU64,
    whole_dead: AtomicU64,
    stop: AtomicBool,
    measuring: AtomicBool,
    wlat: Mutex<Vec<u32>>,
    rlat: Mutex<Vec<u32>>,
    copy_us: AtomicU64,        // time spent inside copy_range
    freed_bytes: AtomicU64,    // bytes of segment files removed
    seg_ms: Mutex<Vec<u32>>,   // wall time per compacted segment
    // group commit
    gc: Mutex<(u64, u64)>, // (next_ticket, synced_through)
    gc_cv: Condvar,
    gc_leader: Mutex<()>,
}

fn seg_path(cfg: &Cfg, id: u64, surv: bool) -> PathBuf {
    cfg.dir.join(format!("{}{:010}.seg", if surv { "c" } else { "" }, id))
}

fn new_segment(sh: &Shared, st: &mut Inner, surv: bool) -> u64 {
    let id = st.next_seg; st.next_seg += 1;
    let f = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(seg_path(&sh.cfg, id, surv)).unwrap();
    st.segs.insert(id, Seg { file: Arc::new(f), len: 0, live: 0, sealed: false, survivor: surv, newest: Instant::now() });
    id
}

/// Reserve `len` bytes in the fresh (or survivor) stream. Returns (segment, offset, file).
fn reserve(sh: &Shared, st: &mut Inner, len: u64, survivor_stream: bool) -> (u64, u64, Arc<File>) {
    let use_surv = survivor_stream && sh.cfg.layout == "C";
    let cur = if use_surv { st.active_surv } else { st.active };
    let full = st.segs.get(&cur).map(|s| s.len + len > sh.seg_size && s.len > 0).unwrap_or(true);
    let id = if full {
        if let Some(s) = st.segs.get_mut(&cur) { s.sealed = true; }
        let n = new_segment(sh, st, use_surv);
        if use_surv { st.active_surv = n } else { st.active = n }
        n
    } else { cur };
    let s = st.segs.get_mut(&id).unwrap();
    let off = s.len;
    s.len += len;
    s.newest = Instant::now();
    st.disk_bytes += len;
    (id, off, Arc::clone(&s.file))
}

fn remove_segment(sh: &Shared, st: &mut Inner, id: u64) {
    if let Some(s) = st.segs.remove(&id) {
        st.disk_bytes -= s.len;
        if sh.measuring.load(Relaxed) { sh.freed_bytes.fetch_add(s.len, Relaxed); }
        let _ = std::fs::remove_file(seg_path(&sh.cfg, id, s.survivor));
    }
}

/// Group commit: one fdatasync covers every write that got a ticket before it started.
/// `segs` are marked dirty *before* the ticket is taken. A leader reads its target
/// ticket before taking the dirty list, so any leader whose target covers this
/// ticket also syncs these segments.
fn durable(sh: &Shared, segs: &[u64]) {
    {
        let mut st = sh.st.lock().unwrap();
        for id in segs { if !st.dirty.contains(id) { st.dirty.push(*id); } }
    }
    let my = { let mut g = sh.gc.lock().unwrap(); g.0 += 1; g.0 };
    loop {
        {
            let g = sh.gc.lock().unwrap();
            if g.1 >= my { return; }
        }
        if let Ok(_lead) = sh.gc_leader.try_lock() {
            let target = sh.gc.lock().unwrap().0;
            let files: Vec<Arc<File>> = {
                let mut st = sh.st.lock().unwrap();
                let d = std::mem::take(&mut st.dirty);
                d.iter().filter_map(|id| st.segs.get(id).map(|s| Arc::clone(&s.file))).collect()
            };
            for f in files { sync(&f); }
            let mut g = sh.gc.lock().unwrap();
            g.1 = g.1.max(target);
            sh.gc_cv.notify_all();
        } else {
            let g = sh.gc.lock().unwrap();
            if g.1 >= my { return; }
            let _ = sh.gc_cv.wait_timeout(g, Duration::from_millis(5)).unwrap();
        }
    }
}

fn sync(f: &File) {
    #[cfg(target_os = "linux")]
    unsafe { libc::fdatasync(f.as_raw_fd()); }
    #[cfg(not(target_os = "linux"))]
    { let _ = f.sync_data(); }
}

// ------------------------------------------------------------------ workload
fn obj_size(r: &mut Rng) -> u64 {
    let x = r.f();
    if x < 0.6 { 64 << 10 } else if x < 0.9 { 1 << 20 } else { 4 << 20 }
}

/// Mean lifetimes (in bytes written) of hot and cold objects, scaled so the
/// steady-state live set is `live_gb`. Cold objects live ~33x longer.
fn lifetimes(cfg: &Cfg) -> (f64, f64) {
    let live = cfg.live_gb * (1u64 << 30) as f64;
    let mix = cfg.hot_frac * 0.3 + (1.0 - cfg.hot_frac) * 10.0;
    (0.3 * live / mix, 10.0 * live / mix)
}

/// Write the steady-state live set up front. Lifetimes are exponential, so a
/// live object's remaining life has the same distribution as a new one's, and
/// cold objects make up their stationary share of live bytes.
fn prefill(sh: &Shared) {
    let (hot_mean, cold_mean) = lifetimes(&sh.cfg);
    let h = sh.cfg.hot_frac;
    let p_cold = (1.0 - h) * cold_mean / ((1.0 - h) * cold_mean + h * hot_mean);
    let target = (sh.cfg.live_gb * (1u64 << 30) as f64) as u64;
    let mut r = Rng(sh.cfg.seed * 1_000_003 + 17);
    let buf = vec![0x5Au8; 4 << 20];
    let mut n: u64 = 0;
    let mut segs = Vec::new();
    loop {
        let mut st = sh.st.lock().unwrap();
        if st.live_bytes >= target { break; }
        let len = obj_size(&mut r);
        let (seg, off, f) = reserve(sh, &mut st, len, false);
        f.write_all_at(&buf[..len as usize], off).unwrap();
        let life = if r.f() < p_cold { r.exp(cold_mean) } else { r.exp(hot_mean) } as u64;
        let id = (255u64 << 48) | n; n += 1;
        let pos = st.ids.len();
        st.ids.push(id);
        st.objs.insert(id, Obj { seg, off, len, pos });
        st.segs.get_mut(&seg).unwrap().live += len;
        st.live_bytes += len;
        st.deaths.push(Reverse((life, id)));
        if !segs.contains(&seg) { segs.push(seg); }
    }
    durable(sh, &segs);
}

fn writer(sh: Arc<Shared>, wid: u64) {
    let mut r = Rng(sh.cfg.seed * 7919 + wid * 104729 + 1);
    let buf = vec![0xA5u8; 4 << 20];
    let (hot_mean, cold_mean) = lifetimes(&sh.cfg);
    while !sh.stop.load(Relaxed) {
        let len = obj_size(&mut r);
        let t0 = Instant::now();
        let (seg, off, f) = { let mut st = sh.st.lock().unwrap(); reserve(&sh, &mut st, len, false) };
        f.write_all_at(&buf[..len as usize], off).unwrap();
        durable(&sh, &[seg]);
        let born = sh.clock.fetch_add(len, Relaxed) + len;
        let life = if r.f() < sh.cfg.hot_frac { r.exp(hot_mean) } else { r.exp(cold_mean) } as u64;
        let id = (wid << 48) | (born & ((1 << 48) - 1));
        {
            let mut st = sh.st.lock().unwrap();
            let pos = st.ids.len();
            st.ids.push(id);
            st.objs.insert(id, Obj { seg, off, len, pos });
            if let Some(s) = st.segs.get_mut(&seg) { s.live += len; }
            st.live_bytes += len;
            st.deaths.push(Reverse((born + life, id)));
            // Expire everything whose lifetime has passed.
            let now = sh.clock.load(Relaxed);
            while let Some(Reverse((d, oid))) = st.deaths.peek().copied() {
                if d > now { break; }
                st.deaths.pop();
                delete(&sh, &mut st, oid);
            }
        }
        sh.fg_bytes.fetch_add(len, Relaxed);
        sh.fg_ops.fetch_add(1, Relaxed);
        if sh.measuring.load(Relaxed) { sh.wlat.lock().unwrap().push(t0.elapsed().as_micros() as u32); }
    }
}

fn delete(sh: &Shared, st: &mut Inner, id: u64) {
    if let Some(o) = st.objs.remove(&id) {
        let last = *st.ids.last().unwrap();
        st.ids.swap_remove(o.pos);
        if last != id { st.objs.get_mut(&last).unwrap().pos = o.pos; }
        st.live_bytes -= o.len;
        let whole_dead = match st.segs.get_mut(&o.seg) {
            Some(s) => { s.live -= o.len; s.sealed && s.live == 0 }
            None => false,
        };
        // A sealed segment with nothing live is freed by unlinking it, no copy.
        if whole_dead { remove_segment(sh, st, o.seg); sh.whole_dead.fetch_add(1, Relaxed); }
    }
}

fn reader(sh: Arc<Shared>, rid: u64) {
    let mut r = Rng(sh.cfg.seed * 31 + rid * 7 + 3);
    let mut buf = vec![0u8; 4 << 20];
    while !sh.stop.load(Relaxed) {
        let pick = {
            let st = sh.st.lock().unwrap();
            if st.ids.is_empty() { None } else {
                let id = st.ids[(r.next() as usize) % st.ids.len()];
                let o = &st.objs[&id];
                st.segs.get(&o.seg).map(|s| (Arc::clone(&s.file), o.off, o.len))
            }
        };
        let Some((f, off, len)) = pick else { std::thread::sleep(Duration::from_millis(1)); continue };
        let t0 = Instant::now();
        // The file may have been unlinked after the lookup; the open handle still reads.
        let _ = f.read_exact_at(&mut buf[..len as usize], off);
        sh.rd_ops.fetch_add(1, Relaxed);
        if sh.measuring.load(Relaxed) { sh.rlat.lock().unwrap().push(t0.elapsed().as_micros() as u32); }
    }
}

// ------------------------------------------------------------------ compactor
fn pick_victim(sh: &Shared, st: &Inner) -> Option<u64> {
    let now = Instant::now();
    let mut best: Option<(f64, u64)> = None;
    for (&id, s) in &st.segs {
        if !s.sealed || s.len == 0 { continue; }
        let u = s.live as f64 / s.len as f64;
        if u >= sh.cfg.umax { continue; }
        let score = match sh.cfg.policy.as_str() {
            "greedy" => 1.0 - u,
            _ => {
                let age = now.duration_since(s.newest).as_secs_f64() + 1.0;
                (1.0 - u) * age / (1.0 + u)
            }
        };
        if best.map(|(b, _)| score > b).unwrap_or(true) { best = Some((score, id)); }
    }
    best.map(|(_, id)| id)
}

fn copy_range(src: &File, src_off: u64, dst: &File, dst_off: u64, len: u64, how: &str, buf: &mut [u8]) {
    #[cfg(target_os = "linux")]
    if how == "cfr" {
        let (mut i, mut o) = (src_off as i64, dst_off as i64);
        let mut left = len as usize;
        while left > 0 {
            let n = unsafe { libc::copy_file_range(src.as_raw_fd(), &mut i, dst.as_raw_fd(), &mut o, left, 0) };
            if n <= 0 { panic!("copy_file_range failed: {}", std::io::Error::last_os_error()); }
            left -= n as usize;
        }
        return;
    }
    let _ = how;
    src.read_exact_at(&mut buf[..len as usize], src_off).unwrap();
    dst.write_all_at(&buf[..len as usize], dst_off).unwrap();
}

fn compactor(sh: Arc<Shared>) {
    let mut buf = vec![0u8; 4 << 20];
    let rate = sh.cfg.rate_mbps * 1e6;
    let mut tokens = 0.0f64;
    let mut last = Instant::now();
    while !sh.stop.load(Relaxed) {
        let victim = { let st = sh.st.lock().unwrap(); pick_victim(&sh, &st) };
        let Some(v) = victim else { std::thread::sleep(Duration::from_millis(20)); continue };
        let t_start = Instant::now();
        sh.compacting.store(true, Relaxed);
        let (src, moves) = {
            let st = sh.st.lock().unwrap();
            let src = Arc::clone(&st.segs[&v].file);
            let m: Vec<(u64, u64, u64)> = st.objs.iter().filter(|(_, o)| o.seg == v).map(|(&id, o)| (id, o.off, o.len)).collect();
            (src, m)
        };
        let mut placed = Vec::with_capacity(moves.len());
        for (id, off, len) in moves {
            if rate > 0.0 {
                let now = Instant::now();
                tokens = (tokens + now.duration_since(last).as_secs_f64() * rate).min(rate * 0.1);
                last = now;
                if tokens < len as f64 {
                    std::thread::sleep(Duration::from_secs_f64((len as f64 - tokens) / rate));
                    tokens = 0.0; last = Instant::now();
                } else { tokens -= len as f64; }
            }
            let (dseg, doff, dst) = { let mut st = sh.st.lock().unwrap(); reserve(&sh, &mut st, len, true) };
            let tc = Instant::now();
            copy_range(&src, off, &dst, doff, len, &sh.cfg.copy, &mut buf);
            if sh.measuring.load(Relaxed) { sh.copy_us.fetch_add(tc.elapsed().as_micros() as u64, Relaxed); }
            sh.comp_bytes.fetch_add(len, Relaxed);
            placed.push((id, off, len, dseg, doff));
        }
        let dsegs: Vec<u64> = { let mut d: Vec<u64> = placed.iter().map(|p| p.3).collect(); d.sort(); d.dedup(); d };
        durable(&sh, &dsegs); // one sync for the batch
        {
            let mut st = sh.st.lock().unwrap();
            for (id, off, len, dseg, doff) in placed {
                let still_here = st.objs.get(&id).map(|o| o.seg == v && o.off == off).unwrap_or(false);
                if still_here {
                    let o = st.objs.get_mut(&id).unwrap();
                    o.seg = dseg; o.off = doff;
                    if let Some(s) = st.segs.get_mut(&dseg) { s.live += len; }
                    if let Some(s) = st.segs.get_mut(&v) { s.live -= len; }
                }
            }
            remove_segment(&sh, &mut st, v);
        }
        sh.segs_compacted.fetch_add(1, Relaxed);
        if sh.measuring.load(Relaxed) { sh.seg_ms.lock().unwrap().push(t_start.elapsed().as_millis() as u32); }
        sh.comp_busy_us.fetch_add(t_start.elapsed().as_micros() as u64, Relaxed);
        sh.compacting.store(false, Relaxed);
    }
}

// ------------------------------------------------------------------ metrics
/// Bytes the filesystem reports as used (counts shared reflink blocks once).
fn fs_used(dir: &std::path::Path) -> u64 {
    use std::ffi::CString;
    let c = CString::new(dir.as_os_str().to_string_lossy().as_bytes()).unwrap();
    let mut v: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut v) } != 0 { return 0; }
    (v.f_blocks as u64 - v.f_bfree as u64) * v.f_frsize as u64
}

/// User + system CPU seconds used by this process so far.
fn cpu_secs() -> f64 {
    let mut u: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) };
    let t = |tv: libc::timeval| tv.tv_sec as f64 + tv.tv_usec as f64 / 1e6;
    t(u.ru_utime) + t(u.ru_stime)
}

fn pct(v: &mut Vec<u32>, p: f64) -> u32 {
    if v.is_empty() { return 0; }
    v.sort_unstable();
    v[((v.len() - 1) as f64 * p).round() as usize]
}

fn main() {
    let cfg = parse();
    let _ = std::fs::remove_dir_all(&cfg.dir);
    std::fs::create_dir_all(&cfg.dir).unwrap();
    let seg_size = cfg.segment_mb << 20;
    let sh = Arc::new(Shared {
        cfg: cfg.clone(), seg_size,
        st: Mutex::new(Inner { objs: HashMap::new(), ids: Vec::new(), segs: BTreeMap::new(), active: u64::MAX, active_surv: u64::MAX,
            next_seg: 0, deaths: BinaryHeap::new(), dirty: Vec::new(), disk_bytes: 0, live_bytes: 0 }),
        clock: AtomicU64::new(0), fg_bytes: AtomicU64::new(0), comp_bytes: AtomicU64::new(0), fg_ops: AtomicU64::new(0),
        rd_ops: AtomicU64::new(0), compacting: AtomicBool::new(false), comp_busy_us: AtomicU64::new(0),
        segs_compacted: AtomicU64::new(0), whole_dead: AtomicU64::new(0), stop: AtomicBool::new(false),
        measuring: AtomicBool::new(false), wlat: Mutex::new(Vec::new()), rlat: Mutex::new(Vec::new()),
        copy_us: AtomicU64::new(0), freed_bytes: AtomicU64::new(0), seg_ms: Mutex::new(Vec::new()),
        gc: Mutex::new((0, 0)), gc_cv: Condvar::new(), gc_leader: Mutex::new(()),
    });
    let fs_base = fs_used(&cfg.dir);
    let tp = Instant::now();
    prefill(&sh);
    eprintln!("prefilled {:.2} GB in {:.1}s", sh.st.lock().unwrap().live_bytes as f64 / (1u64 << 30) as f64, tp.elapsed().as_secs_f64());
    let mut hs = Vec::new();
    for w in 0..cfg.writers { let s = Arc::clone(&sh); hs.push(std::thread::spawn(move || writer(s, w as u64))); }
    for r in 0..cfg.readers { let s = Arc::clone(&sh); hs.push(std::thread::spawn(move || reader(s, r as u64))); }
    if cfg.compact { let s = Arc::clone(&sh); hs.push(std::thread::spawn(move || compactor(s))); }

    let live_target = (cfg.live_gb * (1u64 << 30) as f64) as u64;
    let warm_until = (cfg.warm_x * live_target as f64) as u64;
    let end_at = warm_until + (cfg.churn_x * live_target as f64) as u64;
    let mut tl = cfg.timeline.as_ref().map(|p| {
        let f = File::create(p).unwrap();
        use std::io::Write;
        let mut f = std::io::BufWriter::new(f);
        writeln!(f, "t,clock_gb,disk_gb,live_gb,sa,phys_sa,fg_mbs,comp_mbs,wr_ops,rd_ops,compacting,files,wp99_us,rp99_us").unwrap();
        f
    });
    let t0 = Instant::now();
    let (mut pfg, mut pcomp, mut pw, mut pr) = (0u64, 0u64, 0u64, 0u64);
    let mut m_start: Option<(Instant, u64, u64)> = None;
    // Per-second samples during the measured window: (fg MB/s, compaction active fraction, SA, w p99, r p99)
    let mut samples: Vec<(f64, bool, f64, u32, u32)> = Vec::new();
    let mut phys: Vec<f64> = Vec::new();
    let (mut all_w, mut all_r): (Vec<u32>, Vec<u32>) = (Vec::new(), Vec::new());
    let mut cpu0 = 0.0f64;
    let mut idle_tp = Vec::new(); let mut busy_tp = Vec::new();
    let mut max_files = 0usize;
    let mut busy_prev = 0u64;
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let clock = sh.clock.load(Relaxed);
        if m_start.is_none() && clock >= warm_until {
            m_start = Some((Instant::now(), sh.fg_bytes.load(Relaxed), sh.comp_bytes.load(Relaxed)));
            sh.measuring.store(true, Relaxed);
            cpu0 = cpu_secs();
            sh.wlat.lock().unwrap().clear(); sh.rlat.lock().unwrap().clear();
        }
        let (disk, live, files) = { let st = sh.st.lock().unwrap(); (st.disk_bytes, st.live_bytes, st.segs.len()) };
        max_files = max_files.max(files);
        let (fg, comp, w, r) = (sh.fg_bytes.load(Relaxed), sh.comp_bytes.load(Relaxed), sh.fg_ops.load(Relaxed), sh.rd_ops.load(Relaxed));
        let busy = sh.comp_busy_us.load(Relaxed) + if sh.compacting.load(Relaxed) { 1 } else { 0 };
        let was_busy = busy != busy_prev || sh.compacting.load(Relaxed);
        busy_prev = busy;
        let fg_mbs = (fg - pfg) as f64 / 1e6;
        let comp_mbs = (comp - pcomp) as f64 / 1e6;
        let (wp, rp) = if m_start.is_some() {
            let mut wl = std::mem::take(&mut *sh.wlat.lock().unwrap());
            let mut rl = std::mem::take(&mut *sh.rlat.lock().unwrap());
            let (a, b) = (pct(&mut wl, 0.99), pct(&mut rl, 0.99));
            all_w.extend_from_slice(&wl); all_r.extend_from_slice(&rl);
            (a, b)
        } else { (0, 0) };
        let sa = if live > 0 { disk as f64 / live as f64 } else { 0.0 };
        let psa = if live > 0 { fs_used(&cfg.dir).saturating_sub(fs_base) as f64 / live as f64 } else { 0.0 };
        if m_start.is_some() {
            samples.push((fg_mbs, was_busy, sa, wp, rp));
            phys.push(psa);
            if was_busy || comp_mbs > 0.5 { busy_tp.push(fg_mbs) } else { idle_tp.push(fg_mbs) }
        }
        if let Some(f) = tl.as_mut() {
            use std::io::Write;
            writeln!(f, "{:.0},{:.2},{:.3},{:.3},{:.3},{:.3},{:.1},{:.1},{},{},{},{},{},{}", t0.elapsed().as_secs_f64(), clock as f64 / 1e9,
                disk as f64 / (1u64 << 30) as f64, live as f64 / (1u64 << 30) as f64, sa, psa, fg_mbs, comp_mbs, w - pw, r - pr,
                was_busy as u8, files, wp, rp).unwrap();
        }
        pfg = fg; pcomp = comp; pw = w; pr = r;
        if clock >= end_at { break; }
    }
    sh.stop.store(true, Relaxed);
    for h in hs { h.join().unwrap(); }
    let (ms, fg0, comp0) = m_start.unwrap();
    let secs = ms.elapsed().as_secs_f64();
    let cpu_pct = (cpu_secs() - cpu0) / secs * 100.0;
    let mut segms = std::mem::take(&mut *sh.seg_ms.lock().unwrap());
    let copy_s = sh.copy_us.load(Relaxed) as f64 / 1e6;
    let fgb = (sh.fg_bytes.load(Relaxed) - fg0) as f64;
    let cb = (sh.comp_bytes.load(Relaxed) - comp0) as f64;
    let mean = |v: &[f64]| if v.is_empty() { 0.0 } else { v.iter().sum::<f64>() / v.len() as f64 };
    let sa: Vec<f64> = samples.iter().map(|s| s.2).collect();
    let sa_mean = mean(&sa);
    let sa_max = sa.iter().cloned().fold(0.0, f64::max);
    let mut wps: Vec<u32> = samples.iter().map(|s| s.3).collect();
    let mut rps: Vec<u32> = samples.iter().map(|s| s.4).collect();
    let (it, bt) = (mean(&idle_tp), mean(&busy_tp));
    println!(
        "{{\"segment_mb\":{},\"umax\":{},\"policy\":\"{}\",\"layout\":\"{}\",\"copy\":\"{}\",\"rate_mbps\":{},\"compact\":{},\"live_gb\":{},\
\"secs\":{:.1},\"fg_mbs\":{:.1},\"wa\":{:.3},\"sa_mean\":{:.3},\"sa_max\":{:.3},\"phys_sa_mean\":{:.3},\"wp99_med_us\":{},\"rp99_med_us\":{},\
\"wp99_max_us\":{},\"rp99_max_us\":{},\"fg_idle_mbs\":{:.1},\"fg_busy_mbs\":{:.1},\"rho\":{:.3},\"busy_frac\":{:.3},\
\"segs_compacted\":{},\"whole_dead\":{},\"max_files\":{},\"w_p50_us\":{},\"w_p99_us\":{},\"w_p999_us\":{},\
\"r_p50_us\":{},\"r_p99_us\":{},\"r_p999_us\":{},\"copy_mbs\":{:.1},\"freed_mbs\":{:.1},\"seg_ms_p50\":{},\"seg_ms_max\":{},\"cpu_pct\":{:.1}}}",
        cfg.segment_mb, cfg.umax, cfg.policy, cfg.layout, cfg.copy, cfg.rate_mbps, cfg.compact, cfg.live_gb,
        secs, fgb / 1e6 / secs, (fgb + cb) / fgb.max(1.0), sa_mean, sa_max, mean(&phys), pct(&mut wps, 0.5), pct(&mut rps, 0.5),
        wps.iter().max().copied().unwrap_or(0), rps.iter().max().copied().unwrap_or(0), it, bt,
        if bt > 0.0 { it / bt } else { 1.0 }, busy_tp.len() as f64 / samples.len().max(1) as f64,
        sh.segs_compacted.load(Relaxed), sh.whole_dead.load(Relaxed), max_files,
        pct(&mut all_w, 0.5), pct(&mut all_w, 0.99), pct(&mut all_w, 0.999),
        pct(&mut all_r, 0.5), pct(&mut all_r, 0.99), pct(&mut all_r, 0.999),
        if copy_s > 0.0 { cb / 1e6 / copy_s } else { 0.0 }, sh.freed_bytes.load(Relaxed) as f64 / 1e6 / secs,
        pct(&mut segms, 0.5), segms.iter().max().copied().unwrap_or(0), cpu_pct
    );
    let _ = std::fs::remove_dir_all(&cfg.dir);
}
