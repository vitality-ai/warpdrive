#!/usr/bin/env python3
"""RFC 6 solver: the least disk that still meets the latency and throughput rules.

Usage:
  solve.py <out_dir> --space <dir> [--latency <dir>] [--ceiling-mbs N] [--demand-mbs N]
           [--wp99-ms N] [--rp99-ms N] [--max-wa N]

  --space    runs with writers at full speed (matrix-*.jsonl): space amp, write amp
  --latency  runs at a fixed write rate (matrix-*.jsonl): write and read p99 under load.
             Without it, p99 comes from --space and the output says so.
  --ceiling-mbs  the binding roofline ceiling in device MB/s (disk or CPU, whichever is lower)
  --demand-mbs   customer write MB/s the node must sustain

The decision: one value per setting, as 0/1 variables x[s, v] that sum to 1 per setting.
Each metric is predicted from the baseline plus the measured effect of each chosen value,
paired within each VM. An effect counts only if every VM agrees on its sign AND its median
is larger than the noise floor: the baseline's spread across VMs (max - min). Otherwise it
counts as zero.

    minimize    SA(x)                                   physical disk / live data
    subject to  write p99(x) <= wp99                    at the fixed load
                read  p99(x) <= rp99
                WA(x)        <= min(max_wa, ceiling / demand)
                    (customer capacity = ceiling / WA must cover demand)
                growth(x)    <= max_growth              space amp per GB churned stays flat
                    (compaction keeps up; modeled like the other metrics: baseline
                    plus each value's measured change)
With --vs-nocompact r, the latency limits become r x the no-compaction run's p99:
the question is what compaction adds, not what the write path costs.
"""
import argparse, csv, glob, json, os, statistics
from collections import defaultdict
import numpy as np
from scipy.optimize import milp, LinearConstraint, Bounds

ap = argparse.ArgumentParser()
ap.add_argument("out")
ap.add_argument("--space", required=True)
ap.add_argument("--latency")
ap.add_argument("--ceiling-mbs", type=float)
ap.add_argument("--demand-mbs", type=float)
ap.add_argument("--wp99-ms", type=float, default=20)
ap.add_argument("--rp99-ms", type=float, default=10)
ap.add_argument("--max-wa", type=float, default=2.0)
ap.add_argument("--vs-nocompact", type=float,
                help="latency rule relative to the no-compaction run: p99(x) <= this x p99(no compaction). "
                     "Replaces --wp99-ms/--rp99-ms, for when the write path itself sets the absolute p99.")
ap.add_argument("--max-growth", type=float, default=0.01, help="max space-amp growth per GB churned")
a = ap.parse_args()
os.makedirs(a.out, exist_ok=True)

FACTORS = {
    "segment_mb": ([32, 64, 128, 256, 512, 1024], 256),
    "umax": ([0.2, 0.3, 0.4, 0.5, 0.6, 0.7], 0.5),
    "policy": (["greedy", "costbenefit"], "costbenefit"),
    "layout": (["A", "C"], "C"),
    "copy": (["rw", "cfr"], "cfr"),
    "rate_mbps": ([25, 50, 100, 200, 0], 50),
}
run_name = lambda f, v: "base" if v == FACTORS[f][1] else f"{f}-{v}"
cand = [(f, v) for f, (vals, _) in FACTORS.items() for v in vals]


def device_wa(d, name, trial, fg_mbs):
    """Device write MB/s (iostat, measured window) / customer MB/s. None without iostat."""
    f = os.path.join(d, "mon", f"{name}-t{trial}.iostat")
    if not os.path.exists(f) or fg_mbs <= 0:
        return None
    hdr, w = None, []
    for l in open(f):
        t = l.split()
        if t and t[0].startswith("Device"):
            hdr = t
        elif hdr and t and t[0].startswith("nvme"):
            w.append(float(dict(zip(hdr, t))["wMB/s"]) * 1.048576)
    w = w[len(w) // 3:]
    return statistics.mean(w) / fg_mbs if w else None


def load(d):
    by = defaultdict(dict)
    for fn in sorted(glob.glob(os.path.join(d, "matrix-*.jsonl"))):
        for l in open(fn):
            if l.strip():
                r = json.loads(l)
                dwa = device_wa(d, r["name"], r["trial"], r["r"]["fg_mbs"])
                r["r"]["device_wa"] = dwa if dwa is not None else r["r"]["wa"]
                r["r"]["wa_source"] = "device" if dwa is not None else "counted"
                by[r["name"]][r["trial"]] = r["r"]
    return by


def sa_growth(d, name):
    """Change in space amp per measured GB churned, from the per-second timelines
    (median over trials). Above ~0 means compaction isn't keeping up."""
    out = []
    for fn in glob.glob(os.path.join(d, "**", f"{name}-t*.csv"), recursive=True):
        rows = list(csv.DictReader(open(fn)))
        key = "phys_sa" if rows and "phys_sa" in rows[0] else "sa"
        pts = [(float(r["clock_gb"]), float(r[key])) for r in rows if float(r[key]) > 0]
        pts = pts[len(pts) // 3:]  # measured window: skip warm-up
        if len(pts) > 5:
            x, y = np.array(pts).T
            out.append(np.polyfit(x, y, 1)[0])
    return statistics.median(out) if out else None


S = load(a.space)
L = load(a.latency) if a.latency else S
trials_s = sorted({t for n in S.values() for t in n})
M_SPACE = {"sa": lambda r: r["phys_sa_mean"], "wa": lambda r: r["device_wa"]}
M_LAT = {"wp99": lambda r: r.get("w_p99_us", r["wp99_med_us"]) / 1000,
         "rp99": lambda r: r.get("r_p99_us", r["rp99_med_us"]) / 1000}


NOISE = {}


def effects(by, metrics):
    trials = sorted({t for n in by.values() for t in n})
    base = {m: statistics.median(fn(by["base"][t]) for t in trials) for m, fn in metrics.items()}
    noise = {m: max(fn(by["base"][t]) for t in trials) - min(fn(by["base"][t]) for t in trials) for m, fn in metrics.items()}
    NOISE.update(noise)
    D, dropped = {m: np.zeros(len(cand)) for m in metrics}, []
    for i, (f, v) in enumerate(cand):
        n = run_name(f, v)
        if n == "base" or n not in by:
            continue
        for m, fn in metrics.items():
            d = [fn(by[n][t]) - fn(by["base"][t]) for t in trials if t in by[n] and t in by["base"]]
            same_sign = d and (all(x > 0 for x in d) or all(x < 0 for x in d))
            if same_sign and abs(statistics.median(d)) > noise[m]:
                D[m][i] = statistics.median(d)
            elif d:
                dropped.append((f, v, m, d, "below noise" if same_sign else "VMs disagree"))
    return base, D, dropped


Bs, Ds, drop_s = effects(S, M_SPACE)
Bl, Dl, drop_l = effects(L, M_LAT)
B, D = {**Bs, **Bl}, {**Ds, **Dl}
growth = {(f, v): sa_growth(a.space, run_name(f, v)) for f, v in cand}
g_base = sa_growth(a.space, "base") or 0.0
B["growth"] = g_base
D["growth"] = np.array([((growth[c] if growth[c] is not None else g_base) - g_base) for c in cand])
excluded = []

wa_cap = a.max_wa
if a.ceiling_mbs and a.demand_mbs:
    wa_cap = min(wa_cap, a.ceiling_mbs / a.demand_mbs)


if a.vs_nocompact and "nocompact" in L:
    tl = sorted(L["nocompact"])
    nc_w = statistics.median(M_LAT["wp99"](L["nocompact"][t]) for t in tl)
    nc_r = statistics.median(M_LAT["rp99"](L["nocompact"][t]) for t in tl)
    a.wp99_ms, a.rp99_ms = a.vs_nocompact * nc_w, a.vs_nocompact * nc_r


def solve(rp99=None, wp99=None, Dm=None):
    rp99 = a.rp99_ms if rp99 is None else rp99
    wp99 = a.wp99_ms if wp99 is None else wp99
    Dm = Dm or D
    A, lb, ub = [], [], []
    for f in FACTORS:
        A.append([1.0 if cf == f else 0.0 for cf, _ in cand]); lb.append(1); ub.append(1)
    for m, lim in (("wp99", wp99), ("rp99", rp99), ("wa", wa_cap), ("growth", a.max_growth)):
        A.append(list(Dm[m])); lb.append(-np.inf); ub.append(lim - B[m])
    ubx = np.array([0.0 if c in excluded else 1.0 for c in cand])
    res = milp(Dm["sa"], constraints=LinearConstraint(np.array(A), lb, ub),
               integrality=np.ones(len(cand)), bounds=Bounds(np.zeros(len(cand)), ubx))
    if not res.success:
        return None
    x = np.round(res.x).astype(int)
    return {f: v for (f, v), xi in zip(cand, x) if xi}, {m: B[m] + float(Dm[m] @ x) for m in ("sa", "wa", "wp99", "rp99", "growth")}


fmt = lambda p: " | ".join(str(p[f]) if not (f == "rate_mbps" and p[f] == 0) else "unlimited" for f in FACTORS)
hdr = "| " + " | ".join(FACTORS) + " | pred. space amp | pred. WA | pred. write p99 ms | pred. read p99 ms |\n|" + "---|" * (len(FACTORS) + 4) + "\n"
with open(os.path.join(a.out, "solve.md"), "w") as out:
    out.write(f"Space and WA from `{a.space}`. Latency from `{a.latency or a.space}`"
              + ("" if a.latency else " (**writers at full speed, not a fixed load: p99 here is queueing-dominated, a stand-in only**)") + ".\n\n")
    out.write((f"Latency rule: p99 <= {a.vs_nocompact} x the no-compaction run (write {a.wp99_ms:.1f} ms, read {a.rp99_ms:.1f} ms). " if a.vs_nocompact else "")
              + f"Rules: write p99 <= {a.wp99_ms:.1f} ms, read p99 <= {a.rp99_ms:.1f} ms, space-amp growth <= {a.max_growth} per GB, WA <= {wa_cap:.2f}"
              + (f" (= min({a.max_wa}, ceiling {a.ceiling_mbs:.0f} / demand {a.demand_mbs:.0f}))" if a.ceiling_mbs and a.demand_mbs else "") + ".\n\n")
    src = {r["wa_source"] for n in S.values() for r in n.values()}
    out.write(f"WA source: {', '.join(sorted(src))} (device = iostat writes / customer writes).\n\n")
    out.write(f"Baseline: space amp {B['sa']:.2f}, WA {B['wa']:.2f}, write p99 {B['wp99']:.1f} ms, read p99 {B['rp99']:.1f} ms.\n\n")
    r = solve()
    out.write("**Pick:**\n\n" + hdr)
    out.write(("| " + fmt(r[0]) + f" | {r[1]['sa']:.2f} | {r[1]['wa']:.2f} | {r[1]['wp99']:.1f} | {r[1]['rp99']:.1f} |\n") if r else "| infeasible: no setting meets every rule |\n")
    # Pareto: least disk at each read p99 target.
    out.write("\n**Least disk at each read p99 target** (write p99 and WA rules unchanged):\n\n| read p99 target ms | " + hdr[2:])
    for t in [5, 8, 10, 15, 20, 25, 30, 50]:
        rr = solve(rp99=t)
        out.write(f"| {t} | " + (fmt(rr[0]) + f" | {rr[1]['sa']:.2f} | {rr[1]['wa']:.2f} | {rr[1]['wp99']:.1f} | {rr[1]['rp99']:.1f} |\n" if rr else "infeasible |" + " |" * (len(FACTORS) + 3) + "\n"))
    # Sensitivity: scale each setting's measured effects by 0.7 and 1.3; does the pick change?
    if r:
        out.write("\n**Sensitivity** (each setting's measured effects scaled by 0.7 and 1.3, one setting at a time):\n\n| setting | x0.7 pick changes? | x1.3 pick changes? |\n|---|---|---|\n")
        for f in FACTORS:
            row = []
            for k in (0.7, 1.3):
                Dm = {m: np.array([v * (k if cf == f else 1.0) for v, (cf, _) in zip(D[m], cand)]) for m in D}
                rr = solve(Dm=Dm)
                row.append("infeasible" if not rr else ("no" if rr[0] == r[0] else "yes: " + ", ".join(f"{g}={rr[0][g]}" for g in FACTORS if rr[0][g] != r[0][g])))
            out.write(f"| {f} | {row[0]} | {row[1]} |\n")
    out.write(f"\n**Space-amp growth per GB churned** (baseline {g_base:+.3f}; the rule keeps the pick at or below {a.max_growth}):\n\n")
    out.write("".join(f"- {f} = {v}: {growth[(f, v)]:+.3f}\n" for f, v in cand if growth[(f, v)] is not None))
    out.write("\n**Noise floor** (baseline max - min across VMs): " + ", ".join(f"{m} {v:.3g}" for m, v in NOISE.items()) + "\n")
    out.write("\n**Effects counted as zero:**\n\n| setting | value | metric | per-VM differences | why |\n|---|---|---|---|---|\n")
    for f, v, m, d, why in drop_s + drop_l:
        out.write(f"| {f} | {v} | {m} | {', '.join(f'{x:+.3g}' for x in d)} | {why} |\n")
print(open(os.path.join(a.out, "solve.md")).read())
