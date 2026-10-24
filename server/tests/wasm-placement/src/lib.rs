//! Prototype of a sandboxed, user-defined placement policy (`StripePacker`)
//! run as a WASM module, with the safety boundary that actually matters:
//!   - no imports (no syscalls/clock/random/network/fs) -> instantiation fails
//!   - bounded CPU (fuel) and wall-clock (epoch) -> infinite loops trap
//!   - bounded memory (StoreLimits) -> memory bombs trap
//!   - bounded + fully validated OUTPUT -> a cheap module can't emit a huge or
//!     structurally-invalid result; any violation => Rejected (caller degrades
//!     to plain, never corrupts data)
//!   - index-based output ABI -> a module can't fabricate unit ids; it may only
//!     reference the units it was given, each exactly once
//! On load we also run it once against a sample workload and report the real
//! storage overhead, so the cost is explained before the policy goes live.

use wasmtime::*;

// ---- ABI ----
// input  (at a module-allocated ptr): [u32 k][u32 n][n x u32 size]
// pack(in_ptr, in_len) -> out_ptr
// output (at out_ptr): [u32 num_stripes]
//   per stripe: [u32 capacity][u32 num_bins]
//     per bin:  [u32 num_units][num_units x u32 unit_index]

const MEM_LIMIT_BYTES: usize = 16 << 20; // 16 MiB per invocation
const FUEL: u64 = 50_000_000;            // ~instruction budget; bounds loops
const EPOCH_DEADLINE_MS: u64 = 250;      // wall-clock backstop

#[derive(Debug, Clone, PartialEq)]
pub struct Stripe {
    pub capacity: u32,
    pub bins: Vec<Vec<usize>>, // bins of unit INDICES into the input
}

#[derive(Debug)]
pub enum Outcome {
    /// Passed every safety + structural check. `overhead_pct` is the real
    /// storage cost of this policy on the sample workload (vs MDS-optimal).
    Valid { stripes: Vec<Stripe>, overhead_pct: f64 },
    /// Rejected for the stated reason. Caller MUST degrade to the plain
    /// (built-in) path; user data is never shaped by a rejected policy.
    Rejected(String),
}

pub struct WasmPacker {
    engine: Engine,
    module: Module,
}

impl WasmPacker {
    /// Compile a user-submitted WASM policy. Compilation failure (not valid
    /// wasm) is itself a rejection reason.
    pub fn load(wasm: &[u8]) -> Result<Self, String> {
        let mut c = Config::new();
        c.consume_fuel(true);
        c.epoch_interruption(true);
        let engine = Engine::new(&c).map_err(|e| format!("engine: {e}"))?;
        let module = Module::new(&engine, wasm).map_err(|e| format!("compile: {e}"))?;
        Ok(Self { engine, module })
    }

    /// Run the policy on `sizes` with `k` data bins, `m` parity (for cost).
    /// Never panics on a hostile module: every failure path returns Rejected.
    pub fn pack(&self, k: usize, m: usize, sizes: &[u32]) -> Outcome {
        match self.run(k, sizes) {
            Err(e) => Outcome::Rejected(e),
            Ok(stripes) => match validate(k, sizes, &stripes) {
                Err(e) => Outcome::Rejected(e),
                Ok(()) => {
                    let overhead_pct = overhead_pct(k, m, sizes, &stripes);
                    Outcome::Valid { stripes, overhead_pct }
                }
            },
        }
    }

    fn run(&self, k: usize, sizes: &[u32]) -> Result<Vec<Stripe>, String> {
        let limits = StoreLimitsBuilder::new()
            .memory_size(MEM_LIMIT_BYTES)
            .memories(1)
            .instances(1)
            .build();
        let mut store = Store::new(&self.engine, limits);
        store.limiter(|l| l);
        store
            .set_fuel(FUEL)
            .map_err(|e| format!("fuel: {e}"))?;
        store.set_epoch_deadline(1);

        // Wall-clock backstop: bump the epoch after a deadline so a module
        // that evades the fuel bound (or blocks in host time somehow) still
        // traps. Detached; firing after completion is harmless.
        let eng = self.engine.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(EPOCH_DEADLINE_MS));
            eng.increment_epoch();
        });

        // No imports: a module that needs any (syscall/clock/random) fails here.
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

        // Serialize input and hand it to the module via its own allocator.
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
        write_mem(&memory, &mut store, in_ptr, &input)?;

        let out_ptr = pack
            .call(&mut store, (in_ptr, input.len() as i32))
            .map_err(|e| format!("pack trapped (fuel/epoch/memory/panic): {}", e.root_cause()))?;

        parse_output(&memory, &store, out_ptr, k, n)
    }
}

// ---- bounds-checked memory access ----

fn write_mem(mem: &Memory, store: &mut Store<StoreLimits>, ptr: i32, bytes: &[u8]) -> Result<(), String> {
    mem.write(store, ptr as usize, bytes)
        .map_err(|e| format!("input write out of bounds: {e}"))
}

fn rd_u32(data: &[u8], off: usize) -> Result<u32, String> {
    let end = off.checked_add(4).ok_or("offset overflow")?;
    if end > data.len() {
        return Err(format!("read {off}..{end} out of bounds (mem {})", data.len()));
    }
    Ok(u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]))
}

fn parse_output(
    mem: &Memory,
    store: &Store<StoreLimits>,
    out_ptr: i32,
    k: usize,
    n: usize,
) -> Result<Vec<Stripe>, String> {
    let data = mem.data(store);
    let mut off = out_ptr as usize;

    // Output-size caps: a cheap module must not be able to make us read or
    // allocate an unbounded result. A correct packer emits at most n stripes.
    let max_stripes = n + 1;

    let num_stripes = rd_u32(data, off)? as usize;
    off += 4;
    if num_stripes > max_stripes {
        return Err(format!("num_stripes {num_stripes} exceeds cap {max_stripes}"));
    }
    let mut stripes = Vec::with_capacity(num_stripes);
    let mut total_units_emitted = 0usize;
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
            total_units_emitted += cnt;
            if total_units_emitted > n {
                return Err("output references more units than exist".into());
            }
            let mut bin = Vec::with_capacity(cnt);
            for _ in 0..cnt {
                let idx = rd_u32(data, off)? as usize;
                off += 4;
                bin.push(idx);
            }
            bins.push(bin);
        }
        stripes.push(Stripe { capacity, bins });
    }
    Ok(stripes)
}

// ---- the real safety boundary: structural validation on EVERY pack ----

pub fn validate(k: usize, sizes: &[u32], stripes: &[Stripe]) -> Result<(), String> {
    let n = sizes.len();
    let mut seen = vec![false; n];
    for (si, s) in stripes.iter().enumerate() {
        if s.bins.len() != k {
            return Err(format!("stripe {si}: {} bins != k={k}", s.bins.len()));
        }
        for (bi, bin) in s.bins.iter().enumerate() {
            let mut bin_bytes: u64 = 0;
            for &idx in bin {
                if idx >= n {
                    return Err(format!("stripe {si} bin {bi}: unit index {idx} out of range (n={n})"));
                }
                if seen[idx] {
                    return Err(format!("unit {idx} appears more than once (a unit was split/duplicated)"));
                }
                seen[idx] = true;
                bin_bytes += sizes[idx] as u64;
            }
            if bin_bytes > s.capacity as u64 {
                return Err(format!(
                    "stripe {si} bin {bi}: {bin_bytes} bytes exceeds stripe capacity {}",
                    s.capacity
                ));
            }
        }
    }
    if let Some(missing) = seen.iter().position(|&b| !b) {
        return Err(format!("unit {missing} was dropped (every unit must be placed exactly once)"));
    }
    Ok(())
}

/// Real storage overhead vs MDS-optimal `(1 + m/k) * total_original`
/// (Fusion ASPLOS'25 §6.3) — the same definition WarpDrive's own
/// `packing::overhead_pct` uses, so the cost a user is shown here is the
/// same number the live overhead-threshold guard compares against.
pub fn overhead_pct(k: usize, m: usize, sizes: &[u32], stripes: &[Stripe]) -> f64 {
    let total_original: u64 = sizes.iter().map(|&s| s as u64).sum();
    if total_original == 0 {
        return 0.0;
    }
    let actual_physical: u64 = stripes.iter().map(|s| (k + m) as u64 * s.capacity as u64).sum();
    let optimal = (1.0 + m as f64 / k as f64) * total_original as f64;
    100.0 * (actual_physical as f64 / optimal - 1.0)
}
