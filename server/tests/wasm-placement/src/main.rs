use wasm_placement::*;
const OUT: &str = "fixtures";
fn main() {
    let sizes: Vec<u32> = vec![1000, 800, 600, 400, 200, 100, 50, 10];
    let (k, m) = (3usize, 2usize);
    // (module, expect_valid, required substring in the rejection reason)
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
    println!("\n=== WASM placement-policy safety suite  (k={k}, m={m}, n={} units) ===", sizes.len());
    let mut all_pass = true;
    for (name, expect_valid, need) in cases {
        let bytes = std::fs::read(format!("{OUT}/{name}.wasm")).expect("read wasm");
        let t = std::time::Instant::now();
        let outcome = match WasmPacker::load(&bytes) {
            Err(e) => Outcome::Rejected(format!("load/compile: {e}")),
            Ok(p) => p.pack(k, m, &sizes),
        };
        let dt_ms = t.elapsed().as_secs_f64() * 1000.0;
        let (got_valid, reason, detail) = match &outcome {
            Outcome::Valid { overhead_pct, .. } =>
                (true, String::new(), format!("VALID  -> storage overhead {:.0}% vs optimal", overhead_pct)),
            Outcome::Rejected(r) => (false, r.clone(), format!("REJECTED -> {r}")),
        };
        // rigorous: right verdict AND (for rejections) the RIGHT reason
        let reason_ok = *expect_valid || need.is_empty() || reason.contains(need);
        let pass = got_valid == *expect_valid && reason_ok;
        if !pass && got_valid == *expect_valid && !reason_ok {
            println!("[FAIL] {:17} {:6.1}ms  rejected for WRONG reason (wanted substring {:?}): {}", name, dt_ms, need, detail);
        } else {
            println!("[{}] {:17} {:6.1}ms  {}", if pass { "PASS" } else { "FAIL" }, name, dt_ms, detail);
        }
        all_pass &= pass;
    }
    println!("\n{}", if all_pass {
        "ALL PASS: every hostile module was contained for the RIGHT reason; the valid policy was accepted and its cost explained."
    } else { "FAILURE: a case did not behave as required." });
    std::process::exit(if all_pass { 0 } else { 1 });
}
