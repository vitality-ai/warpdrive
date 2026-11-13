"""
Run lance_demo.py's real row-batch nprobe=4/16/40 measurement N times
against a given cluster URL, parsing its printed medians each time, for
a mean/stdev across independent trials (fresh dataset build + upload +
query each time) instead of a single run.

Usage: python3 run_lance_reps.py <cluster_base_url> <n_reps> <out_json>
"""
import json
import re
import subprocess
import sys

cluster_url = sys.argv[1]
n_reps = int(sys.argv[2])
out_path = sys.argv[3]

pat_plain = re.compile(r"lanceplain: median ([\d.]+)ms")
pat_packed = re.compile(r"lanceivf: median ([\d.]+)ms")

nprobes = [4, 16, 40]
results = {f"nprobe{p}_plain_ms": [] for p in nprobes}
results.update({f"nprobe{p}_packed_ms": [] for p in nprobes})
results["raw_logs"] = []


def parse(out: str):
    section = None
    vals = {}
    for line in out.splitlines():
        for p in nprobes:
            if f"=== nprobe={p} ===" in line:
                section = p
        if section is not None:
            if (m := pat_plain.search(line)):
                vals[f"nprobe{section}_plain_ms"] = float(m.group(1))
            elif (m := pat_packed.search(line)):
                vals[f"nprobe{section}_packed_ms"] = float(m.group(1))
    return vals


for i in range(n_reps):
    print(f"--- rep {i+1}/{n_reps} ---", flush=True)
    proc = subprocess.run(
        [sys.executable, "lance_demo.py", cluster_url],
        capture_output=True, text=True, timeout=120,
    )
    out = proc.stdout + proc.stderr
    results["raw_logs"].append(out)
    vals = parse(out)
    expected = {f"nprobe{p}_plain_ms" for p in nprobes} | {f"nprobe{p}_packed_ms" for p in nprobes}
    if proc.returncode != 0 or not expected.issubset(vals.keys()):
        print(f"  rep {i+1} FAILED (exit={proc.returncode}); tail:")
        print("\n".join(out.splitlines()[-15:]))
        continue
    for k, v in vals.items():
        results[k].append(v)
    print(f"  {vals}")

with open(out_path, "w") as f:
    json.dump(results, f, indent=2)


def summarize(name, vals):
    if not vals:
        print(f"{name}: no successful reps")
        return
    n = len(vals)
    mean = sum(vals) / n
    var = sum((v - mean) ** 2 for v in vals) / n if n > 1 else 0.0
    print(f"{name}: n={n} mean={mean:.1f}ms stdev={var**0.5:.1f}ms")
    return mean


print("\n=== SUMMARY ===")
means = {}
for p in nprobes:
    means[f"plain{p}"] = summarize(f"nprobe={p} plain", results[f"nprobe{p}_plain_ms"])
    means[f"packed{p}"] = summarize(f"nprobe={p} packed", results[f"nprobe{p}_packed_ms"])
    if means[f"plain{p}"] and means[f"packed{p}"]:
        print(f"  nprobe={p} speedup: {means[f'plain{p}']/means[f'packed{p}']:.2f}x")
