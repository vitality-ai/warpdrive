#!/usr/bin/env python3
"""plot_sweep.py <run dir>: PUT and GET MB/s per object size, #244 head against the PR, from summary.csv."""
import csv, sys
import matplotlib.pyplot as plt
import numpy as np

run = sys.argv[1]
rows = list(csv.DictReader(open(f"{run}/summary.csv")))
sizes = sorted({int(r["size"]) for r in rows})
label = {4096: "4 KB", 65536: "64 KB", 1048576: "1 MB", 8388608: "8 MB", 67108864: "64 MB", 268435456: "256 MB"}
dur = 60.0

def mbps(arm, phase, size):
    r = next(r for r in rows if r["arm"] == arm and r["phase"] == phase and int(r["size"]) == size)
    return int(r["ops"]) * size / dur / 1048576  # from the op count, not the rounded rate

plt.rcParams.update({"font.size": 10})
fig, axes = plt.subplots(1, 2, figsize=(11, 4.2), dpi=150)
x = np.arange(len(sizes)); w = 0.38
for ax, phase in zip(axes, ["put", "get"]):
    b = [mbps("base", phase, s) for s in sizes]; p = [mbps("pr", phase, s) for s in sizes]
    ax.bar(x - w / 2, b, w - 0.03, color="#9E9E9E", label="#244 head (df476de)", zorder=3)
    ax.bar(x + w / 2, p, w - 0.03, color="#0072B2", label="this PR", zorder=3)
    for i in range(len(sizes)):
        ax.text(x[i] + w / 2, p[i] * 1.08, f"{(p[i] / b[i] - 1) * 100:+.0f}%", ha="center", fontsize=8, color="#333")
    ax.set_yscale("log"); ax.set_xticks(x, [label[s] for s in sizes])
    ax.set_title(f"{phase.upper()} throughput (MB/s, log scale)", loc="left", fontsize=11)
    ax.grid(axis="y", color="#e5e5e5", zorder=0)
    for sp in ("top", "right"): ax.spines[sp].set_visible(False)
h, l = axes[0].get_legend_handles_labels(); fig.legend(h, l, frameon=False, loc="upper right", ncol=2, fontsize=9)
fig.suptitle("RS(3,2), 5 x n2-standard-2, RocksDB metadata; 60 s per phase; every request to node 0", fontsize=9, color="#555", x=0.01, ha="left")
fig.tight_layout(rect=(0, 0, 1, 0.95))
fig.savefig(f"{run}/sizes.png", facecolor="white")
