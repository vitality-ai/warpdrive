//! Bucket-creation placement policies as submitted compute units.
//!
//! A customer creates a bucket and submits a *tiny compute unit* — a WASM
//! module implementing `StripePacker` — that decides how that bucket's objects
//! are striped. Before the bucket is accepted, the module is run against a
//! sample workload inside a sandbox and WarpDrive produces a full **policy
//! report** so the customer sees exactly what they're signing up for:
//!   - storage cost (overhead vs optimal, padding waste), and how it compares
//!     to the built-in default policy;
//!   - the durability/blast-radius tradeoff (how many objects share a stripe,
//!     i.e. how many are lost together if a stripe is lost);
//!   - the policy's OWN resource cost (fuel, wall time, peak memory) — because
//!     this compute unit runs on every PUT, so its cost is a real, metered
//!     part of the bucket's write path;
//!   - a determinism verdict (required, or the quoted cost wouldn't hold in
//!     production);
//!   - a concrete rendering of how the sample workload actually got placed.
//!
//! Safety is unchanged and non-negotiable: no imports, bounded fuel/epoch,
//! bounded memory, bounded+validated output, index-based ABI, and structural
//! validation on every pack. A submission that fails any check is Rejected and
//! the bucket is refused — a rejected policy never shapes real data.

use wasmtime::*;

const MEM_LIMIT_BYTES: usize = 16 << 20;
const FUEL: u64 = 50_000_000;
const EPOCH_DEADLINE_MS: u64 = 250;

#[derive(Debug, Clone, PartialEq)]
pub struct Stripe {
    pub capacity: u32,
    pub bins: Vec<Vec<usize>>, // bins of unit INDICES
}

/// What the policy's own compute unit costs to run once (it runs per PUT).
#[derive(Debug, Clone, Default)]
pub struct ResourceCost {
    pub fuel: u64,
    pub wall_micros: u128,
    pub peak_mem_bytes: usize,
}

/// Storage cost of the resulting layout.
#[derive(Debug, Clone)]
pub struct StorageCost {
    pub overhead_pct: f64,
    pub num_stripes: usize,
    pub physical_bytes: u64,
    pub padding_waste_bytes: u64,
}

/// The durability/blast-radius tradeoff: objects sharing a stripe are lost
/// together if that stripe is lost (more than m shard failures). Fewer units
/// per stripe = better isolation but higher overhead.
#[derive(Debug, Clone)]
pub struct Durability {
    pub max_units_per_stripe: usize,
    pub avg_units_per_stripe: f64,
}

#[derive(Debug, Clone)]
pub enum Verdict {
    Accepted,
    Rejected(String),
}

#[derive(Debug, Clone)]
pub struct PolicyReport {
    pub bucket: String,
    pub verdict: Verdict,
    pub deterministic: bool,
    pub cost: ResourceCost,
    pub storage: Option<StorageCost>,
    pub durability: Option<Durability>,
    /// Overhead of the built-in default policy on the same workload, so the
    /// customer sees their submission relative to not customizing at all.
    pub default_overhead_pct: f64,
    pub sample_stripes: Vec<Stripe>,
    pub sizes: Vec<u32>,
    pub k: usize,
    pub m: usize,
}

impl PolicyReport {
    pub fn accepted(&self) -> bool {
        matches!(self.verdict, Verdict::Accepted)
    }
}

pub struct WasmPacker {
    engine: Engine,
    module: Module,
}

impl WasmPacker {
    pub fn load(wasm: &[u8]) -> Result<Self, String> {
        let mut c = Config::new();
        c.consume_fuel(true);
        c.epoch_interruption(true);
        let engine = Engine::new(&c).map_err(|e| format!("engine: {e}"))?;
        let module = Module::new(&engine, wasm).map_err(|e| format!("compile: {e}"))?;
        Ok(Self { engine, module })
    }

    /// The bucket-creation entry point: run the submitted policy once against a
    /// sample workload and produce the full report. Never panics on a hostile
    /// module — every failure becomes `Verdict::Rejected`.
    pub fn explain(&self, bucket: &str, k: usize, m: usize, sizes: &[u32]) -> PolicyReport {
        let default_overhead_pct = overhead_pct(k, m, sizes, &fac_greedy(k, sizes));
        let mut report = PolicyReport {
            bucket: bucket.to_string(),
            verdict: Verdict::Rejected("not run".into()),
            deterministic: true, // no nondeterministic imports are ever provided
            cost: ResourceCost::default(),
            storage: None,
            durability: None,
            default_overhead_pct,
            sample_stripes: Vec::new(),
            sizes: sizes.to_vec(),
            k,
            m,
        };

        let t = std::time::Instant::now();
        let run = self.run(k, sizes);
        report.cost.wall_micros = t.elapsed().as_micros();

        match run {
            Err(e) => {
                report.verdict = Verdict::Rejected(e);
            }
            Ok((stripes, cost)) => {
                report.cost.fuel = cost.fuel;
                report.cost.peak_mem_bytes = cost.peak_mem_bytes;
                match validate(k, sizes, &stripes) {
                    Err(e) => report.verdict = Verdict::Rejected(e),
                    Ok(()) => {
                        report.storage = Some(storage_cost(k, m, sizes, &stripes));
                        report.durability = Some(durability(&stripes));
                        report.sample_stripes = stripes;
                        report.verdict = Verdict::Accepted;
                    }
                }
            }
        }
        report
    }

    fn run(&self, k: usize, sizes: &[u32]) -> Result<(Vec<Stripe>, ResourceCost), String> {
        let limits = StoreLimitsBuilder::new()
            .memory_size(MEM_LIMIT_BYTES)
            .memories(1)
            .instances(1)
            .build();
        let mut store = Store::new(&self.engine, limits);
        store.limiter(|l| l);
        store.set_fuel(FUEL).map_err(|e| format!("fuel: {e}"))?;
        store.set_epoch_deadline(1);

        let eng = self.engine.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(EPOCH_DEADLINE_MS));
            eng.increment_epoch();
        });

        let instance = Instance::new(&mut store, &self.module, &[])
            .map_err(|e| format!("instantiate (imports are forbidden): {e}"))?;
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or("module does not export `memory`")?;
        let alloc = instance
            .get_typed_func::<i32, i32>(&mut store, "alloc")
            .map_err(|_| "module does not export `alloc(i32)->i32`".to_string())?;
        let pack = instance
            .get_typed_func::<(i32, i32), i32>(&mut store, "pack")
            .map_err(|_| "module does not export `pack(i32,i32)->i32`".to_string())?;

        let n = sizes.len();
        let mut input = Vec::with_capacity(8 + 4 * n);
        input.extend_from_slice(&(k as u32).to_le_bytes());
        input.extend_from_slice(&(n as u32).to_le_bytes());
        for s in sizes {
            input.extend_from_slice(&s.to_le_bytes());
        }
        let in_ptr = alloc
            .call(&mut store, input.len() as i32)
            .map_err(|e| format!("alloc trapped: {}", e.root_cause()))?;
        memory
            .write(&mut store, in_ptr as usize, &input)
            .map_err(|e| format!("input write out of bounds: {e}"))?;

        let out_ptr = pack
            .call(&mut store, (in_ptr, input.len() as i32))
            .map_err(|e| format!("pack trapped (fuel/epoch/memory/panic): {}", e.root_cause()))?;

        let cost = ResourceCost {
            fuel: FUEL.saturating_sub(store.get_fuel().unwrap_or(0)),
            wall_micros: 0, // filled by caller around the whole run
            peak_mem_bytes: memory.data_size(&store),
        };
        let stripes = parse_output(&memory, &store, out_ptr, k, n)?;
        Ok((stripes, cost))
    }
}

// ---- bounds-checked output parsing ----

fn rd_u32(data: &[u8], off: usize) -> Result<u32, String> {
    let end = off.checked_add(4).ok_or("offset overflow")?;
    if end > data.len() {
        return Err(format!("read {off}..{end} out of bounds (mem {})", data.len()));
    }
    Ok(u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]))
}

fn parse_output(mem: &Memory, store: &Store<StoreLimits>, out_ptr: i32, k: usize, n: usize) -> Result<Vec<Stripe>, String> {
    let data = mem.data(store);
    let mut off = out_ptr as usize;
    let max_stripes = n + 1;
    let num_stripes = rd_u32(data, off)? as usize;
    off += 4;
    if num_stripes > max_stripes {
        return Err(format!("num_stripes {num_stripes} exceeds cap {max_stripes}"));
    }
    let mut stripes = Vec::with_capacity(num_stripes);
    let mut total = 0usize;
    for _ in 0..num_stripes {
        let capacity = rd_u32(data, off)?;
        off += 4;
        let num_bins = rd_u32(data, off)? as usize;
        off += 4;
        if num_bins != k {
            return Err(format!("stripe has {num_bins} bins, must equal k={k}"));
        }
        let mut bins = Vec::with_capacity(num_bins);
        for _ in 0..num_bins {
            let cnt = rd_u32(data, off)? as usize;
            off += 4;
            if cnt > n {
                return Err(format!("bin claims {cnt} units, only {n} exist"));
            }
            total += cnt;
            if total > n {
                return Err("output references more units than exist".into());
            }
            let mut bin = Vec::with_capacity(cnt);
            for _ in 0..cnt {
                bin.push(rd_u32(data, off)? as usize);
                off += 4;
            }
            bins.push(bin);
        }
        stripes.push(Stripe { capacity, bins });
    }
    Ok(stripes)
}

// ---- the real safety boundary: structural validation on every pack ----

pub fn validate(k: usize, sizes: &[u32], stripes: &[Stripe]) -> Result<(), String> {
    let n = sizes.len();
    let mut seen = vec![false; n];
    for (si, s) in stripes.iter().enumerate() {
        if s.bins.len() != k {
            return Err(format!("stripe {si}: {} bins != k={k}", s.bins.len()));
        }
        for (bi, bin) in s.bins.iter().enumerate() {
            let mut bytes: u64 = 0;
            for &idx in bin {
                if idx >= n {
                    return Err(format!("stripe {si} bin {bi}: unit index {idx} out of range (n={n})"));
                }
                if seen[idx] {
                    return Err(format!("unit {idx} appears more than once (a unit was split/duplicated)"));
                }
                seen[idx] = true;
                bytes += sizes[idx] as u64;
            }
            if bytes > s.capacity as u64 {
                return Err(format!("stripe {si} bin {bi}: {bytes} bytes exceeds stripe capacity {}", s.capacity));
            }
        }
    }
    if let Some(missing) = seen.iter().position(|&b| !b) {
        return Err(format!("unit {missing} was dropped (every unit must be placed exactly once)"));
    }
    Ok(())
}

// ---- cost + durability analytics ----

pub fn overhead_pct(k: usize, m: usize, sizes: &[u32], stripes: &[Stripe]) -> f64 {
    let total: u64 = sizes.iter().map(|&s| s as u64).sum();
    if total == 0 {
        return 0.0;
    }
    let physical: u64 = stripes.iter().map(|s| (k + m) as u64 * s.capacity as u64).sum();
    let optimal = (1.0 + m as f64 / k as f64) * total as f64;
    100.0 * (physical as f64 / optimal - 1.0)
}

fn storage_cost(k: usize, m: usize, sizes: &[u32], stripes: &[Stripe]) -> StorageCost {
    let physical: u64 = stripes.iter().map(|s| (k + m) as u64 * s.capacity as u64).sum();
    // padding waste = physical data-shard bytes minus real bytes (parity excluded)
    let data_physical: u64 = stripes.iter().map(|s| k as u64 * s.capacity as u64).sum();
    let real: u64 = sizes.iter().map(|&s| s as u64).sum();
    StorageCost {
        overhead_pct: overhead_pct(k, m, sizes, stripes),
        num_stripes: stripes.len(),
        physical_bytes: physical,
        padding_waste_bytes: data_physical.saturating_sub(real),
    }
}

fn durability(stripes: &[Stripe]) -> Durability {
    let per: Vec<usize> = stripes.iter().map(|s| s.bins.iter().map(|b| b.len()).sum()).collect();
    let max = per.iter().copied().max().unwrap_or(0);
    let avg = if per.is_empty() { 0.0 } else { per.iter().sum::<usize>() as f64 / per.len() as f64 };
    Durability { max_units_per_stripe: max, avg_units_per_stripe: avg }
}

/// Built-in default policy (Fusion Algorithm 1, generalized) — the baseline a
/// submission is compared against, so a customer sees the cost of customizing
/// vs not. Ported to operate on indices for this prototype.
pub fn fac_greedy(k: usize, sizes: &[u32]) -> Vec<Stripe> {
    let mut idx: Vec<usize> = (0..sizes.len()).collect();
    idx.sort_by(|&a, &b| sizes[b].cmp(&sizes[a]));
    let mut remaining = idx;
    let mut stripes = Vec::new();
    while !remaining.is_empty() {
        let seed = remaining.remove(0);
        let capacity = sizes[seed];
        let mut bins: Vec<Vec<usize>> = vec![Vec::new(); k];
        bins[0].push(seed);
        let mut bin_sizes = vec![0u64; k];
        bin_sizes[0] = sizes[seed] as u64;
        let mut still = Vec::new();
        for u in remaining {
            let cand = (1..k)
                .filter(|&b| bin_sizes[b] + sizes[u] as u64 <= capacity as u64)
                .min_by_key(|&b| bin_sizes[b]);
            match cand {
                Some(b) => {
                    bins[b].push(u);
                    bin_sizes[b] += sizes[u] as u64;
                }
                None => still.push(u),
            }
        }
        stripes.push(Stripe { capacity, bins });
        remaining = still;
    }
    stripes
}

/// Human-readable submission report — what a customer sees at bucket creation.
pub fn render(r: &PolicyReport) -> String {
    use std::fmt::Write;
    let mut o = String::new();
    let _ = writeln!(o, "┌─ Placement policy submission for bucket \"{}\"", r.bucket);
    let _ = writeln!(o, "│  workload sample: {} objects, RS(k={}, m={})", r.sizes.len(), r.k, r.m);
    match &r.verdict {
        Verdict::Rejected(why) => {
            let _ = writeln!(o, "│  VERDICT: REJECTED — bucket not created");
            let _ = writeln!(o, "│  reason: {why}");
            let _ = writeln!(o, "│  (a rejected policy never shapes real data; fix and resubmit)");
        }
        Verdict::Accepted => {
            let s = r.storage.as_ref().unwrap();
            let d = r.durability.as_ref().unwrap();
            let delta = s.overhead_pct - r.default_overhead_pct;
            let _ = writeln!(o, "│  VERDICT: ACCEPTED");
            let _ = writeln!(o, "│");
            let _ = writeln!(o, "│  STORAGE COST");
            let _ = writeln!(o, "│    overhead vs optimal : {:.1}%", s.overhead_pct);
            let _ = writeln!(o, "│    vs default policy    : {:.1}% ({}{:.1}% — {})",
                r.default_overhead_pct, if delta >= 0.0 { "+" } else { "" }, delta,
                if delta > 0.1 { "your policy costs MORE" } else if delta < -0.1 { "your policy costs LESS" } else { "about the same" });
            let _ = writeln!(o, "│    stripes              : {}", s.num_stripes);
            let _ = writeln!(o, "│    physical bytes        : {} ({} padding waste)", s.physical_bytes, s.padding_waste_bytes);
            let _ = writeln!(o, "│");
            let _ = writeln!(o, "│  DURABILITY / BLAST RADIUS");
            let _ = writeln!(o, "│    objects per stripe    : max {}, avg {:.1}", d.max_units_per_stripe, d.avg_units_per_stripe);
            let _ = writeln!(o, "│    => up to {} object(s) are lost together if a stripe is lost", d.max_units_per_stripe);
            let _ = writeln!(o, "│       (more objects/stripe = cheaper storage but larger correlated-loss blast radius)");
            let _ = writeln!(o, "│");
            let _ = writeln!(o, "│  POLICY COMPUTE COST (runs on every PUT)");
            let _ = writeln!(o, "│    fuel (≈instructions)  : {}", r.cost.fuel);
            let _ = writeln!(o, "│    wall time             : {} µs", r.cost.wall_micros);
            let _ = writeln!(o, "│    peak sandbox memory   : {} KiB", r.cost.peak_mem_bytes / 1024);
            let _ = writeln!(o, "│    deterministic         : {} (no clock/RNG/host access)", r.deterministic);
            let _ = writeln!(o, "│");
            let _ = writeln!(o, "│  SAMPLE PLACEMENT (first {} stripe(s))", r.sample_stripes.len().min(3));
            for (si, st) in r.sample_stripes.iter().take(3).enumerate() {
                let bins: Vec<String> = st.bins.iter()
                    .map(|b| format!("[{}]", b.iter().map(|&i| format!("{}B", r.sizes[i])).collect::<Vec<_>>().join(",")))
                    .collect();
                let _ = writeln!(o, "│    stripe {si} cap={} : {}", st.capacity, bins.join(" "));
            }
        }
    }
    let _ = writeln!(o, "└─");
    o
}
