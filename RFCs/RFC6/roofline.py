#!/usr/bin/env python3
"""RFC 6 roofline: how close each compaction configuration gets to the
hardware limits, at each machine size.

Usage: roofline.py <out_dir> <label>=<results_dir> [<label>=<results_dir> ...]

Each results_dir has ceilings.jsonl (fio) and scale.jsonl (segbench runs).

Every customer byte written costs WA bytes of device writes, so
    customer write MB/s  <=  device write ceiling / WA
The x axis is customer bytes per device byte written (1 / WA), so the disk
ceiling is a line through the origin with slope = the fio write bandwidth.

The CPU ceiling comes from the runs themselves: CPU % per MB/s of device
traffic (writes + compaction + reads), so the most device traffic the
machine's cores can carry is  cores * 100 / (CPU % per MB/s). Reads run at
a fixed rate, so their share comes off the top before dividing by WA.
"""
import json, os, statistics, sys
from collections import defaultdict
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt

OUT = sys.argv[1]
os.makedirs(OUT, exist_ok=True)
MEAN_OBJ_MB = (0.6 * 64 * 1024 + 0.3 * (1 << 20) + 0.1 * (4 << 20)) / 1e6  # segbench object mix
READ_RATE = 200  # reads per second, segbench default for these runs

sizes = []
for arg in sys.argv[2:]:
    label, d = arg.split("=", 1)
    ceil = {j["test"]: j for j in (json.loads(l) for l in open(os.path.join(d, "ceilings.jsonl")))}
    runs = [json.loads(l) for l in open(os.path.join(d, "scale.jsonl")) if l.strip()]
    cores = int(open(os.path.join(d, "ceilings.txt")).read().split("=")[1])
    sizes.append((label, cores, ceil, runs))

PAL = ["#2a78d6", "#eb6834", "#1baf7a", "#eda100"]
INK, MUTED, GRID = "#1f2328", "#5b636e", "#e3e6ea"
plt.rcParams.update({"font.size": 9, "axes.edgecolor": GRID, "axes.labelcolor": MUTED, "xtick.color": MUTED,
                     "ytick.color": MUTED, "axes.grid": True, "grid.color": GRID, "axes.spines.top": False,
                     "axes.spines.right": False, "figure.facecolor": "white"})
fig, axes = plt.subplots(1, len(sizes), figsize=(5.2 * len(sizes), 4.6), squeeze=False)
table = []
for k, (label, cores, ceil, runs) in enumerate(sizes):
    ax = axes[0, k]
    w_ceiling = ceil["write_sync"]["write_mbs"]
    read_mbs = READ_RATE * MEAN_OBJ_MB
    by = defaultdict(list)
    for r in runs:
        by[r["name"]].append(r["r"])
    pts = []
    for name, rs in by.items():
        wa = statistics.median(x["wa"] for x in rs)
        fg = statistics.median(x["fg_mbs"] for x in rs)
        cpu = statistics.median(x["cpu_pct"] for x in rs)
        device = fg * wa + read_mbs
        pts.append((name, 1 / wa, fg, cpu, device))
    # CPU cost per MB/s of device traffic: median over compacting runs.
    c_per = statistics.median(p[3] / p[4] for p in pts if p[4] > 0)
    cpu_device_cap = cores * 100 / c_per
    xs = [i / 100 for i in range(20, 101)]
    ax.plot(xs, [w_ceiling * x for x in xs], color=INK, lw=1.6, label=f"disk: fio write+sync {w_ceiling:.0f} MB/s")
    ax.plot(xs, [max(cpu_device_cap - read_mbs, 0) * x for x in xs], color=MUTED, lw=1.6, ls="--",
            label=f"CPU: {cores} cores, {c_per:.2f}% per MB/s")
    for name, x, y, cpu, dev in pts:
        nc = name == "nocompact"
        ax.plot([x], [y], marker="s" if nc else "o", ms=6, color=PAL[1] if nc else PAL[0], mec="white", mew=1)
        ax.annotate(name, (x, y), textcoords="offset points", xytext=(4, 3), fontsize=6.5, color=MUTED)
        bound = min(w_ceiling * x, max(cpu_device_cap - read_mbs, 0) * x)
        limiter = "disk" if w_ceiling * x <= max(cpu_device_cap - read_mbs, 0) * x else "CPU"
        table.append((label, name, 1 / x, y, cpu, bound, y / bound if bound else 0, limiter))
    ax.set_xlabel("customer bytes per device byte written (1 / WA)")
    ax.set_ylabel("customer write MB/s")
    ax.set_title(f"{label}", loc="left", color=INK)
    ax.set_xlim(0.45, 1.05); ax.set_ylim(bottom=0)
    ax.legend(loc="upper left", fontsize=7, frameon=False)
fig.suptitle("Roofline: each run against the disk and CPU ceilings (lower ceiling binds)", x=0.01, ha="left", color=INK)
fig.tight_layout(rect=(0, 0, 1, 0.94))
fig.savefig(os.path.join(OUT, "roofline.png"), dpi=150)

with open(os.path.join(OUT, "roofline.md"), "w") as f:
    f.write("| Size | Run | WA | customer MB/s | process CPU % | ceiling MB/s | share of ceiling | binding ceiling |\n|---|---|---|---|---|---|---|---|\n")
    for row in sorted(table):
        f.write("| {} | {} | {:.2f} | {:.0f} | {:.0f} | {:.0f} | {:.0%} | {} |\n".format(*row))
    for label, cores, ceil, _ in sizes:
        f.write(f"\n{label}: " + ", ".join(f"{t} {c['write_mbs'] or c['read_mbs']:.0f} MB/s" for t, c in ceil.items()))
print(open(os.path.join(OUT, "roofline.md")).read())
