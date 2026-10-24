use wasm_placement::*;

const OUT: &str = "fixtures";

fn load_report(name: &str, bucket: &str, k: usize, m: usize, sizes: &[u32]) -> PolicyReport {
    let bytes = std::fs::read(format!("{OUT}/{name}.wasm")).expect("read wasm");
    match WasmPacker::load(&bytes) {
        Err(e) => PolicyReport {
            bucket: bucket.into(), verdict: Verdict::Rejected(format!("load/compile: {e}")),
            deterministic: true, cost: ResourceCost::default(), storage: None, durability: None,
            default_overhead_pct: overhead_pct(k, m, sizes, &fac_greedy(k, sizes)),
            sample_stripes: vec![], sizes: sizes.to_vec(), k, m,
        },
        Ok(p) => p.explain(bucket, k, m, sizes),
    }
}

fn reason(r: &PolicyReport) -> String {
    match &r.verdict { Verdict::Rejected(s) => s.clone(), Verdict::Accepted => String::new() }
}

fn main() {
    // a varied sample workload (object sizes in bytes)
    let sizes: Vec<u32> = vec![1000, 800, 600, 400, 200, 100, 50, 10];
    let (k, m) = (3usize, 2usize);

    // 1) The accepted submission, rendered in full — what a customer sees at
    //    bucket creation after submitting their compute unit.
    println!("{}", render(&load_report("good_identity", "telemetry-cold", k, m, &sizes)));

    // 2) Aggressive adversarial suite: each hostile compute unit must be
    //    REJECTED (bucket refused) for the RIGHT reason.
    let cases: &[(&str, bool, &str)] = &[
        ("good_identity",    true,  ""),
        ("evil_loop",        false, "fuel"),
        ("evil_import",      false, "imports are forbidden"),
        ("evil_split",       false, "more than once"),
        ("evil_oob_index",   false, "out of range"),
        ("evil_overcap",     false, "exceeds stripe capacity"),
        ("evil_huge_output", false, "exceeds cap"),
        ("evil_mem",         false, "trap"),
    ];
    println!("=== adversarial submissions (each must be refused safely) ===");
    let mut all_pass = true;
    for (name, expect_ok, need) in cases {
        let r = load_report(name, "adversary", k, m, &sizes);
        let ok = r.accepted();
        let why = reason(&r);
        let reason_ok = *expect_ok || need.is_empty() || why.contains(need);
        let pass = ok == *expect_ok && reason_ok;
        all_pass &= pass;
        let detail = if ok {
            format!("ACCEPTED (overhead {:.0}%, blast-radius {})",
                r.storage.as_ref().unwrap().overhead_pct, r.durability.as_ref().unwrap().max_units_per_stripe)
        } else if ok == *expect_ok && !reason_ok {
            format!("REJECTED for WRONG reason (wanted {:?}): {}", need, why)
        } else {
            format!("REJECTED -> {}", why)
        };
        println!("[{}] {:17} {}", if pass { "PASS" } else { "FAIL" }, name, detail);
    }
    println!("\n{}", if all_pass {
        "ALL PASS: every hostile compute unit refused for the right reason; the valid policy accepted and fully explained."
    } else { "FAILURE." });
    std::process::exit(if all_pass { 0 } else { 1 });
}
