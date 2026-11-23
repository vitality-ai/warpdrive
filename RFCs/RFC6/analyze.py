#!/usr/bin/env python3
"""RFC 6: turn segbench results into per-factor effects, then choose a
configuration with an ILP.

Usage: analyze.py <results_dir> <out_dir> [validation.jsonl]

results_dir holds matrix-<trial>.jsonl files (one per VM). Each line:
{"name", "fs", "trial", "r": {segbench summary}}.

Model: every metric M is predicted as M(base) plus the main effect of each
factor's chosen value, measured one factor at a time and paired within a VM:
    M(x) = M_base + sum_f sum_v x[f,v] * delta[f,v](M)
The ILP picks exactly one value per factor (binary x[f,v]) to minimize a
weighted sum of five terms, each normalized by its baseline value:
    space:        physical space amplification
    durability of the device: write amplification
    performance:  foreground slowdown (rho), write p99, read p99
A weight sweep shows how the pick moves between space and performance.
"""
import glob, json, os, statistics, sys
from collections import defaultdict

import numpy as np
from scipy.optimize import milp, LinearConstraint, Bounds
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt

RES, OUT = sys.argv[1], sys.argv[2]
VAL = sys.argv[3] if len(sys.argv) > 3 else None
os.makedirs(OUT, exist_ok=True)

def device_wa(name, trial, fg):
    """iostat device writes / customer writes, when the run was monitored."""
    f = os.path.join(RES, "mon", f"{name}-t{trial}.iostat")
    if not os.path.exists(f) or fg <= 0:
        return None
    hdr, w = None, []
    for l in open(f):
        t = l.split()
        if t and t[0].startswith("Device"):
            hdr = t
        elif hdr and t and t[0].startswith("nvme"):
            w.append(float(dict(zip(hdr, t))["wMB/s"]) * 1.048576)
    w = w[len(w) // 3:]
    return statistics.mean(w) / fg if w else None


rows = []
for f in sorted(glob.glob(os.path.join(RES, "matrix-*.jsonl"))):
    rows += [json.loads(l) for l in open(f) if l.strip()]
for r in rows:
    d = device_wa(r["name"], r["trial"], r["r"]["fg_mbs"])
    r["r"]["device_wa"] = d if d is not None else r["r"]["wa"]
WA_SOURCE = "device" if any(device_wa(r["name"], r["trial"], r["r"]["fg_mbs"]) for r in rows[:3]) else "counted"
trials = sorted({r["trial"] for r in rows})
by = defaultdict(dict)  # name -> trial -> summary
for r in rows:
    by[r["name"]][r["trial"]] = r["r"]

FACTORS = {  # factor -> (summary key, values in plot order, baseline value)
    "segment_mb": ("segment_mb", [32, 64, 128, 256, 512, 1024], 256),
    "umax": ("umax", [0.2, 0.3, 0.4, 0.5, 0.6, 0.7], 0.5),
    "policy": ("policy", ["greedy", "costbenefit"], "costbenefit"),
    "layout": ("layout", ["A", "C"], "C"),
    "copy": ("copy", ["rw", "cfr"], "cfr"),
    "rate_mbps": ("rate_mbps", [25, 50, 100, 200, 0], 50),
}


def run_name(f, v):
    return "base" if v == FACTORS[f][2] else f"{f}-{v}"


def rho(name, t):
    """Foreground slowdown against the no-compaction control on the same VM."""
    c, x = by["nocompact"].get(t), by[name].get(t)
    return c["fg_mbs"] / x["fg_mbs"] if c and x and x["fg_mbs"] > 0 else None


METRICS = {
    "phys_sa": lambda n, t: by[n][t]["phys_sa_mean"],
    "sa": lambda n, t: by[n][t]["sa_mean"],
    "wa": lambda n, t: by[n][t]["device_wa"],
    "rho": rho,
    "fg_mbs": lambda n, t: by[n][t]["fg_mbs"],
    "rp99_ms": lambda n, t: by[n][t]["rp99_med_us"] / 1000,
    "wp99_ms": lambda n, t: by[n][t]["wp99_med_us"] / 1000,
    "files": lambda n, t: by[n][t]["max_files"],
}


def med(name, m):
    v = [METRICS[m](name, t) for t in trials if t in by[name]]
    v = [x for x in v if x is not None]
    return statistics.median(v) if v else None


def spread(name, m):
    v = [METRICS[m](name, t) for t in trials if t in by[name]]
    v = [x for x in v if x is not None]
    return (min(v), max(v)) if v else (None, None)


IGNORED = []  # (factor, value, metric, paired diffs): effects the solver treats as zero


def delta(f, v, m, record=False):
    """Median over trials of (metric at f=v) - (metric at base), paired per VM.

    An effect counts only if every VM agrees on its sign. Otherwise it's noise
    and the solver gets 0 for it."""
    n = run_name(f, v)
    d = [METRICS[m](n, t) - METRICS[m]("base", t) for t in trials
         if t in by[n] and t in by["base"] and METRICS[m](n, t) is not None and METRICS[m]("base", t) is not None]
    if not d:
        return None
    if not (all(x > 0 for x in d) or all(x < 0 for x in d)):
        if record:
            IGNORED.append((f, v, m, d))
        return 0.0
    return statistics.median(d)


# ------------------------------------------------------------------ table
with open(os.path.join(OUT, "results.md"), "w") as out:
    out.write(f"Medians over {len(trials)} trials (one VM each). rho = no-compaction foreground MB/s / this run's.\n\n")
    out.write("| Run | phys SA | logical SA | WA | rho | fg MB/s | read p99 ms | write p99 ms | max files |\n|---|---|---|---|---|---|---|---|---|\n")
    names = ["base"] + [run_name(f, v) for f, (_, vals, b) in FACTORS.items() for v in vals if v != b] + ["nocompact", "ext4-copy-cfr", "ext4-copy-rw"]
    for n in names:
        if n not in by:
            continue
        g = lambda m, fmt: (fmt.format(med(n, m)) if med(n, m) is not None else "n/a")
        out.write(f"| {n} | {g('phys_sa','{:.2f}')} | {g('sa','{:.2f}')} | {g('wa','{:.2f}')} | {g('rho','{:.2f}')} | "
                  f"{g('fg_mbs','{:.0f}')} | {g('rp99_ms','{:.1f}')} | {g('wp99_ms','{:.0f}')} | {g('files','{:.0f}')} |\n")

# ------------------------------------------------------------------ plots
PAL = ["#2a78d6", "#eb6834", "#1baf7a", "#eda100"]
INK, MUTED, GRID = "#1f2328", "#5b636e", "#e3e6ea"
plt.rcParams.update({"font.size": 9, "axes.edgecolor": GRID, "axes.labelcolor": MUTED, "xtick.color": MUTED,
                     "ytick.color": MUTED, "axes.grid": True, "grid.color": GRID, "axes.spines.top": False,
                     "axes.spines.right": False, "figure.facecolor": "white"})
PLOT_M = [("phys_sa", "space amp. (physical)"), ("wa", f"write amp. ({WA_SOURCE})"), ("wp99_ms", "write p99, ms (median per-second)"), ("rp99_ms", "read p99, ms (median per-second)")]
# Shared, honest y ranges per metric, so small differences look small.
YLIM = {"phys_sa": None, "wa": (0.95, None), "wp99_ms": (0, None), "rp99_ms": (0, None)}
fig, axes = plt.subplots(len(FACTORS), len(PLOT_M), figsize=(13, 2.3 * len(FACTORS)))
for i, (f, (_, vals, b)) in enumerate(FACTORS.items()):
    labels = [("unlim." if (f == "rate_mbps" and v == 0) else str(v)) for v in vals]
    for j, (m, title) in enumerate(PLOT_M):
        ax = axes[i, j]
        ys, lo, hi = [], [], []
        for v in vals:
            n = run_name(f, v)
            y = med(n, m) if n in by else None
            a, c = spread(n, m) if n in by else (None, None)
            ys.append(y); lo.append(y - a if y is not None else 0); hi.append(c - y if y is not None else 0)
        x = np.arange(len(vals))
        ok = [k for k, y in enumerate(ys) if y is not None]
        ax.errorbar(x[ok], [ys[k] for k in ok], yerr=[[lo[k] for k in ok], [hi[k] for k in ok]], color=PAL[j],
                    marker="os^D"[j], ms=5, lw=2, capsize=3)
        bi = vals.index(b)
        if ys[bi] is not None:
            ax.plot([bi], [ys[bi]], marker="o", ms=10, mfc="none", mec=INK, mew=1.2)
        ax.set_xticks(x, labels)
        allv = [med(n, m) for n in by if med(n, m) is not None]
        if m == "phys_sa":
            ax.set_ylim(0, max(allv) * 1.1)
        elif m == "wa":
            ax.set_ylim(0.95, max(1.3, max(allv) * 1.05))
        else:
            ax.set_ylim(0, max(allv) * 1.1)
        if i == 0:
            ax.set_title(title, loc="left", color=INK)
        if j == 0:
            ax.set_ylabel(f, color=INK)
fig.suptitle("One factor at a time around the baseline (circled). Median of 3 VMs, bars = min-max. Each metric uses one y range for all rows.", x=0.01, ha="left", color=INK)
fig.tight_layout(rect=(0, 0, 1, 0.97))
fig.savefig(os.path.join(OUT, "factors.png"), dpi=150)
plt.close(fig)

# ------------------------------------------------------------------ ILP
cand = [(f, v) for f, (_, vals, _) in FACTORS.items() for v in vals]
D = {m: np.array([(delta(f, v, m, record=True) if v != FACTORS[f][2] else 0.0) or 0.0 for f, v in cand]) for m in METRICS}
B = {m: med("base", m) for m in METRICS}


OBJ = ["phys_sa", "wa", "rho", "wp99_ms", "rp99_ms"]


def solve(w, max_rp99_x=None, min_fg_x=None):
    """w: weights for OBJ, in order. Each term is normalized by its baseline."""
    c = sum(wi * D[m] / B[m] for wi, m in zip(w, OBJ))
    A, lb, ub = [], [], []
    for f in FACTORS:  # exactly one value per factor
        A.append([1.0 if cf == f else 0.0 for cf, _ in cand]); lb.append(1); ub.append(1)
    if max_rp99_x:
        A.append(list(D["rp99_ms"])); lb.append(-np.inf); ub.append(B["rp99_ms"] * max_rp99_x - B["rp99_ms"])
    if min_fg_x:
        A.append(list(D["fg_mbs"])); lb.append(B["fg_mbs"] * min_fg_x - B["fg_mbs"]); ub.append(np.inf)
    res = milp(c, constraints=LinearConstraint(np.array(A), lb, ub), integrality=np.ones(len(cand)), bounds=Bounds(0, 1))
    if not res.success:
        return None
    x = np.round(res.x).astype(int)
    pick = {f: v for (f, v), xi in zip(cand, x) if xi}
    pred = {m: B[m] + float(D[m] @ x) for m in OBJ + ["fg_mbs"]}
    return pick, pred


# Weights for (space amp, write amp, rho, write p99, read p99).
WEIGHTS = [("space-first", (0.5, 0.1, 0.15, 0.1, 0.15)),
           ("balanced", (0.25, 0.15, 0.2, 0.2, 0.2)),
           ("performance-first", (0.1, 0.1, 0.3, 0.25, 0.25))]


def fmt_pick(p):
    return (f"{p['segment_mb']} | {p['umax']} | {p['policy']} | {p['layout']} | {p['copy']} | {p['rate_mbps'] or 'unlimited'}")


picks = []
with open(os.path.join(OUT, "ilp.md"), "w") as out:
    out.write("ILP over measured main effects. Objective terms are normalized by the baseline. "
              "Performance = foreground slowdown (rho), write p99, read p99.\n\n")
    out.write("| Profile | weights (SA, WA, rho, w p99, r p99) | segment MB | umax | policy | layout | copy | rate MB/s | "
              "pred. phys SA | pred. WA | pred. rho | pred. fg MB/s | pred. w p99 ms | pred. r p99 ms |\n")
    out.write("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|\n")
    for name, w in WEIGHTS:
        r = solve(w)
        if not r:
            out.write(f"| {name} | {w} | infeasible |\n"); continue
        p, pr = r
        picks.append((name, p, pr))
        out.write(f"| {name} | {w} | {fmt_pick(p)} | {pr['phys_sa']:.2f} | {pr['wa']:.2f} | {pr['rho']:.2f} | "
                  f"{pr['fg_mbs']:.0f} | {pr['wp99_ms']:.0f} | {pr['rp99_ms']:.1f} |\n")
    # Sweep: performance weight p from 0 to 1, split evenly over rho, w p99, r p99;
    # the rest split 2:1 between space and write amplification.
    out.write("\n**Weight sweep** (share of weight on performance; the rest goes 2:1 to space amp and write amp):\n\n")
    out.write("| performance share | segment MB | umax | policy | layout | copy | rate MB/s | pred. phys SA | pred. rho | pred. r p99 ms |\n")
    out.write("|---|---|---|---|---|---|---|---|---|---|\n")
    sweep = []
    for share in [0.0, 0.2, 0.4, 0.5, 0.6, 0.8, 1.0]:
        rest = 1 - share
        w = (rest * 2 / 3, rest / 3, share / 3, share / 3, share / 3)
        r = solve(w)
        if r:
            p, pr = r
            sweep.append((share, p, pr))
            out.write(f"| {share:.1f} | {fmt_pick(p)} | {pr['phys_sa']:.2f} | {pr['rho']:.2f} | {pr['rp99_ms']:.1f} |\n")
with open(os.path.join(OUT, "ilp.md"), "a") as out:
    out.write("\n**Effects treated as zero** (the VMs disagreed on direction):\n\n")
    out.write("| factor | value | metric | paired differences per VM |\n|---|---|---|---|\n")
    for f, v, m, d in IGNORED:
        if m in OBJ:
            out.write(f"| {f} | {v} | {m} | {', '.join(f'{x:+.3g}' for x in d)} |\n")
json.dump({"profiles": [{"weights": n, "pick": p, "pred": pr} for n, p, pr in picks],
           "sweep": [{"perf_share": sh, "pick": p, "pred": pr} for sh, p, pr in sweep]},
          open(os.path.join(OUT, "ilp.json"), "w"), indent=1)

# ------------------------------------------------------------------ validation
if VAL and os.path.exists(VAL):
    vrows = [json.loads(l) for l in open(VAL) if l.strip()]
    vby = defaultdict(list)
    for r in vrows:
        vby[r["name"]].append(r["r"])
    with open(os.path.join(OUT, "validation.md"), "w") as out:
        out.write("| Run | trials | phys SA | WA | fg MB/s | read p99 ms |\n|---|---|---|---|---|---|\n")
        for n, rs in sorted(vby.items()):
            m = lambda k: statistics.median(x[k] for x in rs)
            out.write(f"| {n} | {len(rs)} | {m('phys_sa_mean'):.2f} | {m('wa'):.2f} | {m('fg_mbs'):.0f} | {m('rp99_med_us')/1000:.1f} |\n")
print(open(os.path.join(OUT, "results.md")).read())
print(open(os.path.join(OUT, "ilp.md")).read())
