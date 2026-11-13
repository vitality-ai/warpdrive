"""
Run tpch_duckdb_benchmark.py's actual Q6 (selective, both buckets) and Q1
(packed-only) measurement N times against a given cluster URL, parsing its
own printed medians each time, to get a mean/stdev across independent
trials instead of a single noisy run. Each trial re-uploads and re-queries
for real -- no caching of results across trials.

Usage: python3 run_tpch_reps.py <cluster_base_url> <n_reps> <out_json>
"""
import json
import re
import subprocess
import sys

cluster_url = sys.argv[1]
n_reps = int(sys.argv[2])
out_path = sys.argv[3]

pat_plain = re.compile(r"tpchplain: median ([\d.]+)ms")
pat_packed = re.compile(r"tpchivf: median ([\d.]+)ms")

results = {"q6_plain_ms": [], "q6_packed_ms": [], "q1_packed_ms": [], "raw_logs": []}


def parse(out: str):
    """Stateful line scan: track which === Qn === section we're in, since
    a 'server-side evidence' line sits between the tpchplain/tpchivf
    lines and broke an earlier single-regex-block attempt (it doesn't
    match across that extra line without re.DOTALL)."""
    section = None
    q6_plain = q6_packed = q1_packed = None
    for line in out.splitlines():
        if "=== Q6 ===" in line:
            section = 6
        elif "=== Q1 ===" in line:
            section = 1
        elif section == 6 and (m := pat_plain.search(line)):
            q6_plain = float(m.group(1))
        elif section == 6 and (m := pat_packed.search(line)):
            q6_packed = float(m.group(1))
        elif section == 1 and (m := pat_packed.search(line)):
            q1_packed = float(m.group(1))
    return q6_plain, q6_packed, q1_packed


for i in range(n_reps):
    print(f"--- rep {i+1}/{n_reps} ---", flush=True)
    proc = subprocess.run(
        [sys.executable, "tpch_duckdb_benchmark.py", cluster_url],
        capture_output=True, text=True, timeout=180,
    )
    out = proc.stdout + proc.stderr
    results["raw_logs"].append(out)
    q6p, q6k, q1k = parse(out)
    if proc.returncode != 0 or q6p is None or q6k is None or q1k is None:
        print(f"  rep {i+1} FAILED (exit={proc.returncode}); tail of output:")
        print("\n".join(out.splitlines()[-15:]))
        continue
    results["q6_plain_ms"].append(q6p)
    results["q6_packed_ms"].append(q6k)
    results["q1_packed_ms"].append(q1k)
    print(f"  Q6 plain={q6p}ms packed={q6k}ms ({q6p/q6k:.1f}x)  Q1 packed={q1k}ms")

with open(out_path, "w") as f:
    json.dump(results, f, indent=2)


def summarize(name, vals):
    if not vals:
        print(f"{name}: no successful reps")
        return
    n = len(vals)
    mean = sum(vals) / n
    var = sum((v - mean) ** 2 for v in vals) / n if n > 1 else 0.0
    print(f"{name}: n={n} mean={mean:.1f}ms stdev={var**0.5:.1f}ms min={min(vals):.1f} max={max(vals):.1f}")


print("\n=== SUMMARY ===")
summarize("Q6 plain", results["q6_plain_ms"])
summarize("Q6 packed", results["q6_packed_ms"])
summarize("Q1 packed", results["q1_packed_ms"])
if results["q6_plain_ms"] and results["q6_packed_ms"]:
    mp = sum(results["q6_plain_ms"]) / len(results["q6_plain_ms"])
    mk = sum(results["q6_packed_ms"]) / len(results["q6_packed_ms"])
    print(f"Q6 speedup (mean plain / mean packed): {mp/mk:.1f}x")
