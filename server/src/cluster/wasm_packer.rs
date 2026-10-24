//! A user-submitted placement policy, run as a sandboxed WASM module behind
//! the existing `StripePacker` trait. From `coordinator.rs`'s point of view it
//! is just another registry entry — no change to the write path — which is the
//! whole reason every component here is a trait with a pluggable impl.
//!
//! Safety is enforced by the sandbox and by the coordinator's
//! `validate_stripes` gate together:
//!   - no imports provided  -> a module that tries to reach the host (syscall,
//!     clock, RNG, network, fs, another tenant's data) fails to instantiate;
//!   - fuel + epoch deadline -> an infinite loop traps;
//!   - `StoreLimits` memory cap -> a memory bomb traps;
//!   - bounded, bounds-checked, index-based output -> a cheap module can't emit
//!     a huge result and can't name a unit it wasn't given.
//! On ANY failure (compile, instantiate, trap, bad ABI, out-of-range output)
//! `pack` returns an empty `Vec<Stripe>`; the coordinator's `validate_stripes`
//! then rejects it ("unit dropped") and degrades to the plain path. A hostile
//! or buggy policy therefore costs a fallback, never data integrity — and its
//! worst case is storage overhead, never durability, because erasure coding is
//! applied across the policy's bins regardless of how it binned them.
//!
//! ABI (little-endian). Input at a module-allocated pointer:
//!   [u32 k][u32 n][n x u32 size][n x u32 locality_key]
//! `size` is the unit's byte length; `locality_key` is the first 4 bytes of
//! the unit's metadata (0 if absent) — enough for a workload-aware policy to
//! group units that are read together, without the module ever seeing content.
//! `pack(in_ptr, in_len) -> out_ptr`. Output at out_ptr:
//!   [u32 num_stripes]{ [u32 capacity][u32 num_bins]{ [u32 num_units]{u32 idx} } }
//! Units are referenced by index into the input, so a module cannot fabricate
//! or rename a unit.

use super::packing::{Stripe, StripePacker, Unit};
use std::sync::OnceLock;
use wasmtime::*;

const MEM_LIMIT_BYTES: usize = 16 << 20;
const FUEL: u64 = 100_000_000;
// Wall-clock budget = EPOCH_TICK_MS * EPOCH_DEADLINE_TICKS (~250ms). Enforced
// by ONE shared epoch ticker thread for the whole process, not a thread per
// pack: micro-benchmark M1 (docs/placement-policy-serving.md) measured the
// thread-per-pack spawn at 68-80% of total pack latency (70-91µs vs 16-29µs),
// and a shared engine also lets modules share a compiled-module cache.
const EPOCH_TICK_MS: u64 = 50;
const EPOCH_DEADLINE_TICKS: u64 = 5;

/// One shared wasm engine for the whole process, with a single background
/// epoch ticker. All `WasmPacker`s compile against it and run on it, so there
/// is exactly one ticker thread regardless of how many buckets have policies.
fn shared_engine() -> &'static Engine {
    static ENGINE: OnceLock<Engine> = OnceLock::new();
    ENGINE.get_or_init(|| {
        let mut c = Config::new();
        c.consume_fuel(true);
        c.epoch_interruption(true);
        let engine = Engine::new(&c).expect("build wasm engine");
        let ticker = engine.clone();
        std::thread::Builder::new()
            .name("wasm-epoch-ticker".into())
            .spawn(move || loop {
                std::thread::sleep(std::time::Duration::from_millis(EPOCH_TICK_MS));
                ticker.increment_epoch();
            })
            .expect("spawn epoch ticker");
        engine
    })
}

pub struct WasmPacker {
    module: Module,
}

impl WasmPacker {
    pub fn from_bytes(wasm: &[u8]) -> Result<Self, String> {
        let module = Module::new(shared_engine(), wasm).map_err(|e| format!("compile: {e}"))?;
        Ok(Self { module })
    }

    pub fn from_file(path: &str) -> Result<Self, String> {
        let bytes = std::fs::read(path).map_err(|e| format!("read {path}: {e}"))?;
        Self::from_bytes(&bytes)
    }

    fn run(&self, k: usize, units: &[Unit]) -> Result<Vec<Stripe>, String> {
        let limits = StoreLimitsBuilder::new()
            .memory_size(MEM_LIMIT_BYTES)
            .memories(1)
            .instances(1)
            .build();
        let mut store = Store::new(shared_engine(), limits);
        store.limiter(|l| l);
        store.set_fuel(FUEL).map_err(|e| format!("fuel: {e}"))?;
        // Trap after EPOCH_DEADLINE_TICKS of the shared ticker (~250ms). No
        // per-pack thread: the one shared ticker advances the epoch for all.
        store.set_epoch_deadline(EPOCH_DEADLINE_TICKS);

        let instance = Instance::new(&mut store, &self.module, &[])
            .map_err(|e| format!("instantiate (imports forbidden): {e}"))?;
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or("no `memory` export")?;
        let alloc = instance
            .get_typed_func::<i32, i32>(&mut store, "alloc")
            .map_err(|_| "no `alloc(i32)->i32` export".to_string())?;
        let pack = instance
            .get_typed_func::<(i32, i32), i32>(&mut store, "pack")
            .map_err(|_| "no `pack(i32,i32)->i32` export".to_string())?;

        // serialize [k][n][sizes][keys]
        let n = units.len();
        let mut input = Vec::with_capacity(8 + 8 * n);
        input.extend_from_slice(&(k as u32).to_le_bytes());
        input.extend_from_slice(&(n as u32).to_le_bytes());
        for u in units {
            input.extend_from_slice(&(u.size as u32).to_le_bytes());
        }
        for u in units {
            let key = u
                .metadata
                .get(0..4)
                .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                .unwrap_or(0);
            input.extend_from_slice(&key.to_le_bytes());
        }

        let in_ptr = alloc
            .call(&mut store, input.len() as i32)
            .map_err(|e| format!("alloc trapped: {}", e.root_cause()))?;
        memory
            .write(&mut store, in_ptr as usize, &input)
            .map_err(|e| format!("input write OOB: {e}"))?;
        let out_ptr = pack
            .call(&mut store, (in_ptr, input.len() as i32))
            .map_err(|e| format!("pack trapped (fuel/epoch/memory/panic): {}", e.root_cause()))?;

        parse_output(&memory, &store, out_ptr, k, units)
    }
}

impl StripePacker for WasmPacker {
    fn pack(&self, k: usize, units: &[Unit]) -> Vec<Stripe> {
        // On any failure return an empty packing; the coordinator's
        // validate_stripes rejects it and degrades to plain. We never panic
        // or write a partial result from a hostile module.
        match self.run(k, units) {
            Ok(stripes) => stripes,
            Err(why) => {
                log::warn!("wasm packer failed, will degrade to plain: {why}");
                Vec::new()
            }
        }
    }
}

fn rd_u32(data: &[u8], off: usize) -> Result<u32, String> {
    let end = off.checked_add(4).ok_or("offset overflow")?;
    if end > data.len() {
        return Err(format!("read {off}..{end} OOB (mem {})", data.len()));
    }
    Ok(u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]))
}

fn parse_output(
    mem: &Memory,
    store: &Store<StoreLimits>,
    out_ptr: i32,
    k: usize,
    units: &[Unit],
) -> Result<Vec<Stripe>, String> {
    let n = units.len();
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
        let capacity = rd_u32(data, off)? as usize;
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
                let idx = rd_u32(data, off)? as usize;
                off += 4;
                if idx >= n {
                    return Err(format!("unit index {idx} out of range (n={n})"));
                }
                bin.push(units[idx].unit_id.clone());
            }
            bins.push(bin);
        }
        stripes.push(Stripe { bins, capacity });
    }
    Ok(stripes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn units(sizes: &[usize]) -> Vec<Unit> {
        sizes
            .iter()
            .enumerate()
            .map(|(i, &s)| Unit { unit_id: format!("u{i}"), size: s, metadata: vec![] })
            .collect()
    }

    // a minimal VALID policy in WAT: one unit per stripe (bin0=[i], rest empty,
    // capacity=size[i]). Exercises the full sandbox + ABI round trip.
    const IDENTITY_WAT: &str = r#"
    (module
      (memory (export "memory") 2)
      (global $off (mut i32) (i32.const 1024))
      (func (export "alloc") (param $n i32) (result i32)
        (local $p i32)
        (local.set $p (global.get $off))
        (global.set $off (i32.add (global.get $off) (local.get $n)))
        (local.get $p))
      (func (export "pack") (param $in i32) (param $len i32) (result i32)
        (local $k i32) (local $n i32) (local $i i32) (local $out i32) (local $p i32) (local $b i32)
        (local.set $k (i32.load (local.get $in)))
        (local.set $n (i32.load (i32.add (local.get $in) (i32.const 4))))
        (local.set $out (global.get $off))
        (local.set $p (local.get $out))
        ;; num_stripes = n
        (i32.store (local.get $p) (local.get $n)) (local.set $p (i32.add (local.get $p) (i32.const 4)))
        (local.set $i (i32.const 0))
        (block $done (loop $L
          (br_if $done (i32.ge_u (local.get $i) (local.get $n)))
          ;; capacity = size[i]  (sizes start at in+8)
          (i32.store (local.get $p)
            (i32.load (i32.add (local.get $in) (i32.add (i32.const 8) (i32.mul (local.get $i) (i32.const 4))))))
          (local.set $p (i32.add (local.get $p) (i32.const 4)))
          ;; num_bins = k
          (i32.store (local.get $p) (local.get $k)) (local.set $p (i32.add (local.get $p) (i32.const 4)))
          ;; bin0 = [i]
          (i32.store (local.get $p) (i32.const 1)) (local.set $p (i32.add (local.get $p) (i32.const 4)))
          (i32.store (local.get $p) (local.get $i)) (local.set $p (i32.add (local.get $p) (i32.const 4)))
          ;; bins 1..k empty
          (local.set $b (i32.const 1))
          (block $bd (loop $BL
            (br_if $bd (i32.ge_u (local.get $b) (local.get $k)))
            (i32.store (local.get $p) (i32.const 0)) (local.set $p (i32.add (local.get $p) (i32.const 4)))
            (local.set $b (i32.add (local.get $b) (i32.const 1)))
            (br $BL)))
          (local.set $i (i32.add (local.get $i) (i32.const 1)))
          (br $L)))
        (global.set $off (local.get $p))
        (local.get $out)))
    "#;

    // a HOSTILE policy: infinite loop. Must trap and yield an empty packing.
    const EVIL_LOOP_WAT: &str = r#"
    (module
      (memory (export "memory") 1)
      (global $off (mut i32) (i32.const 1024))
      (func (export "alloc") (param i32) (result i32) (global.get $off))
      (func (export "pack") (param i32) (param i32) (result i32) (loop (br 0)) (i32.const 0)))
    "#;

    #[test]
    fn valid_policy_round_trips_and_validates() {
        let wasm = wat::parse_str(IDENTITY_WAT).unwrap();
        let p = WasmPacker::from_bytes(&wasm).unwrap();
        let us = units(&[1000, 800, 600, 400]);
        let stripes = p.pack(3, &us);
        assert_eq!(stripes.len(), 4, "identity -> one stripe per unit");
        // the coordinator's gate must accept it
        super::super::packing::validate_stripes(3, &us, &stripes).expect("valid");
    }

    #[test]
    fn hostile_policy_degrades_to_empty_and_is_rejected() {
        let wasm = wat::parse_str(EVIL_LOOP_WAT).unwrap();
        let p = WasmPacker::from_bytes(&wasm).unwrap();
        let us = units(&[1000, 800, 600, 400]);
        let stripes = p.pack(3, &us); // infinite loop -> fuel trap -> empty
        assert!(stripes.is_empty(), "hostile policy yields empty packing");
        // the coordinator's gate then rejects the empty packing -> degrade to plain
        assert!(super::super::packing::validate_stripes(3, &us, &stripes).is_err());
    }
}
