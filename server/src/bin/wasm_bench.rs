//! Micro-benchmarks for serving user-defined WASM placement policies.
//! Each number decides one design choice in docs/placement-policy-serving.md.
//! Build/run: cargo run --release --features wasm --bin wasm_bench

fn main() {
    #[cfg(feature = "wasm")]
    bench::run();
    #[cfg(not(feature = "wasm"))]
    eprintln!("build with the wasm feature: cargo run --release --features wasm --bin wasm_bench");
}

#[cfg(feature = "wasm")]
mod bench {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Instant;
    use wasmtime::*;

    // identity policy (one unit per stripe); reads sizes at in+8, works for any n.
    const IDENTITY_WAT: &str = r#"
    (module
      (memory (export "memory") 4)
      (global $off (mut i32) (i32.const 1024))
      (func (export "alloc") (param $n i32) (result i32)
        (local $p i32) (local.set $p (global.get $off))
        (global.set $off (i32.add (global.get $off) (local.get $n))) (local.get $p))
      (func (export "pack") (param $in i32) (param $len i32) (result i32)
        (local $k i32)(local $n i32)(local $i i32)(local $out i32)(local $p i32)(local $b i32)
        (local.set $k (i32.load (local.get $in)))
        (local.set $n (i32.load (i32.add (local.get $in) (i32.const 4))))
        (local.set $out (global.get $off)) (local.set $p (local.get $out))
        (i32.store (local.get $p) (local.get $n)) (local.set $p (i32.add (local.get $p) (i32.const 4)))
        (local.set $i (i32.const 0))
        (block $d (loop $L (br_if $d (i32.ge_u (local.get $i) (local.get $n)))
          (i32.store (local.get $p) (i32.load (i32.add (local.get $in) (i32.add (i32.const 8) (i32.mul (local.get $i) (i32.const 4))))))
          (local.set $p (i32.add (local.get $p) (i32.const 4)))
          (i32.store (local.get $p) (local.get $k)) (local.set $p (i32.add (local.get $p) (i32.const 4)))
          (i32.store (local.get $p) (i32.const 1)) (local.set $p (i32.add (local.get $p) (i32.const 4)))
          (i32.store (local.get $p) (local.get $i)) (local.set $p (i32.add (local.get $p) (i32.const 4)))
          (local.set $b (i32.const 1))
          (block $bd (loop $BL (br_if $bd (i32.ge_u (local.get $b) (local.get $k)))
            (i32.store (local.get $p) (i32.const 0)) (local.set $p (i32.add (local.get $p) (i32.const 4)))
            (local.set $b (i32.add (local.get $b) (i32.const 1))) (br $BL)))
          (local.set $i (i32.add (local.get $i) (i32.const 1))) (br $L)))
        (global.set $off (local.get $p)) (local.get $out)))
    "#;

    fn input(k: usize, n: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(8 + 8 * n);
        v.extend_from_slice(&(k as u32).to_le_bytes());
        v.extend_from_slice(&(n as u32).to_le_bytes());
        for i in 0..n { v.extend_from_slice(&(((i as u32) % 1000 + 1) * 64).to_le_bytes()); } // sizes
        for _ in 0..n { v.extend_from_slice(&0u32.to_le_bytes()); } // keys
        v
    }

    fn limited_engine() -> Engine {
        let mut c = Config::new();
        c.consume_fuel(true);
        c.epoch_interruption(true);
        Engine::new(&c).unwrap()
    }
    fn plain_engine() -> Engine { Engine::new(&Config::new()).unwrap() }

    // one full pack: fresh Store + Instance + call. `epoch` picks M1 strategy;
    // `limits` toggles the safety machinery (M5).
    #[derive(Clone, Copy)]
    enum Epoch { None, ThreadPerPack, SharedTicker }

    fn one_pack(engine: &Engine, module: &Module, inp: &[u8], limits: bool, epoch: Epoch) {
        if limits {
            let sl = StoreLimitsBuilder::new().memory_size(16 << 20).memories(1).instances(1).build();
            let mut store = Store::new(engine, sl);
            store.limiter(|l| l);
            store.set_fuel(100_000_000).unwrap();
            match epoch {
                Epoch::ThreadPerPack => {
                    // model the real WasmPacker's thread-per-pack: deadline(1)
                    // plus a spawned watchdog. It doesn't increment here (that
                    // only matters for a runaway); we measure the spawn cost,
                    // and this engine gets no stray increments, so no trap.
                    store.set_epoch_deadline(1_000_000);
                    std::thread::spawn(|| { std::thread::sleep(std::time::Duration::from_millis(50)); });
                }
                Epoch::SharedTicker => { store.set_epoch_deadline(5); } // ticker runs on this engine
                Epoch::None => {}
            }
            drive(&mut store, module, inp);
        } else {
            let mut store = Store::new(engine, ());
            drive(&mut store, module, inp);
        }
    }

    fn drive<T>(store: &mut Store<T>, module: &Module, inp: &[u8]) {
        let inst = Instance::new(&mut *store, module, &[]).unwrap();
        let mem = inst.get_memory(&mut *store, "memory").unwrap();
        let alloc = inst.get_typed_func::<i32, i32>(&mut *store, "alloc").unwrap();
        let pack = inst.get_typed_func::<(i32, i32), i32>(&mut *store, "pack").unwrap();
        let p = alloc.call(&mut *store, inp.len() as i32).unwrap();
        mem.write(&mut *store, p as usize, inp).unwrap();
        let out = pack.call(&mut *store, (p, inp.len() as i32)).unwrap();
        // touch the first word so the call isn't optimized away
        let d = mem.data(&*store);
        std::hint::black_box(u32::from_le_bytes([d[out as usize], d[out as usize+1], d[out as usize+2], d[out as usize+3]]));
    }

    fn stats(mut xs: Vec<u128>) -> (f64, f64) {
        xs.sort_unstable();
        let p50 = xs[xs.len()/2] as f64;
        let mean = xs.iter().sum::<u128>() as f64 / xs.len() as f64;
        (p50, mean)
    }

    fn time_loop<F: FnMut()>(iters: usize, mut f: F) -> (f64, f64) {
        for _ in 0..(iters/10).max(50) { f(); } // warmup
        let mut xs = Vec::with_capacity(iters);
        for _ in 0..iters { let t = Instant::now(); f(); xs.push(t.elapsed().as_nanos()); }
        stats(xs)
    }

    pub fn run() {
        let wasm = wat::parse_str(IDENTITY_WAT).unwrap();
        // separate engines so epoch counters never cross-contaminate: the
        // shared ticker runs only on eng_st; eng_tpp gets no increments.
        let eng_tpp = limited_engine();
        let eng_st = limited_engine();
        let plain = plain_engine();
        let mod_tpp = Module::new(&eng_tpp, &wasm).unwrap();
        let mod_st = Module::new(&eng_st, &wasm).unwrap();
        let mod_plain = Module::new(&plain, &wasm).unwrap();
        println!("compiled module size (serialized): {} bytes", mod_st.serialize().map(|b| b.len()).unwrap_or(0));

        // M3 — cold start: compile the module from bytes.
        let (c50, cmean) = time_loop(200, || { std::hint::black_box(Module::new(&eng_st, &wasm).unwrap()); });
        println!("\nM3 cold compile (Module::new)     p50={:.1}µs mean={:.1}µs", c50/1000.0, cmean/1000.0);

        // shared ticker: one thread for the whole process, on eng_st only.
        let stop = Arc::new(AtomicBool::new(false));
        { let e = eng_st.clone(); let s = stop.clone();
          std::thread::spawn(move || { while !s.load(Ordering::Relaxed) { std::thread::sleep(std::time::Duration::from_millis(50)); e.increment_epoch(); } }); }

        println!("\nwarm pack latency (fresh Store+Instance each call), by units/object:");
        println!("{:>6} {:>16} {:>16} {:>14} {:>14}", "n", "thread/pack µs", "shared-tick µs", "no-limits µs", "M1 saving");
        for &n in &[16usize, 64, 256, 1024] {
            let inp = input(3, n);
            let (t50,_) = time_loop(800, || one_pack(&eng_tpp, &mod_tpp, &inp, true, Epoch::ThreadPerPack));
            let (s50,_) = time_loop(2000, || one_pack(&eng_st, &mod_st, &inp, true, Epoch::SharedTicker));
            let (n50,_) = time_loop(2000, || one_pack(&plain, &mod_plain, &inp, false, Epoch::None));
            println!("{:>6} {:>16.2} {:>16.2} {:>14.2} {:>13.0}%", n, t50/1000.0, s50/1000.0, n50/1000.0, (t50-s50)/t50*100.0);
        }

        // M2 — instance-only cost (warm module), to see how much of a pack is instantiate.
        let (i50,_) = time_loop(5000, || {
            let mut store = Store::new(&plain, ());
            std::hint::black_box(Instance::new(&mut store, &mod_plain, &[]).unwrap());
        });
        println!("\nM2 instantiate-only (warm, no limits) p50={:.2}µs", i50/1000.0);

        stop.store(true, Ordering::Relaxed);
        println!("\ninterpretation:");
        println!("  M1: shared ticker removes the per-pack thread spawn (see saving column).");
        println!("  M5: compare thread/pack & shared-tick vs no-limits = cost of the safety machinery.");
        println!("  M3: cold compile is the first-PUT penalty a module cache + pre-compile removes.");
    }
}
