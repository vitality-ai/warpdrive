#!/usr/bin/env python3
"""plot_soak.py <soak dir> <out.png> <title>: node resources over the hour (mean and per node) and the load's
PUT/GET rate and p99 per minute, as separate charts (no shared axes)."""
import csv, glob, sys, statistics as st
import matplotlib; matplotlib.use("Agg"); import matplotlib.pyplot as plt
d, out, title = sys.argv[1], sys.argv[2], sys.argv[3]
COL = ["#2a78d6", "#eb6834", "#1baf7a", "#8a5cd6", "#c9a227"]
nodes = [[list(map(int, r)) for r in csv.reader(open(f)) if len(r) == 9 and r[0].isdigit()] for f in sorted(glob.glob(f"{d}/node[0-9].csv"))]
t0 = min(n[0][0] for n in nodes)
import os
ps = [r for r in csv.DictReader(open(f"{d}/per_second.csv"))] if os.path.exists(f"{d}/per_second.csv") else []
pt0 = min((int(r["unix_s"]) for r in ps), default=0)
fig, axes = plt.subplots(2, 3, figsize=(15, 7.5), dpi=120)
panels = [(1, "Memory (RSS), GB", 1 / 1048576), (4, "Segment data on disk, GB", 1 / 1048576), (6, "Metadata logs, GB", 1 / 1048576),
          (7, "CPU busy, %", 1), (2, "Open file descriptors", 1)]
for ax, (col, label, scale) in zip(axes.flat, panels):
    if col == 6 and max(r[col] for n in nodes for r in n) < 1024:
        ax.set_axis_off(); ax.set_title(label, fontsize=10, loc="left")
        ax.text(0.02, 0.5, "Not sampled: the sampler measures Bitcask's .log files,\nand this run keeps metadata in RocksDB directories.", fontsize=9, color="#555", transform=ax.transAxes)
        continue
    for i, n in enumerate(nodes):
        ax.plot([(r[0] - t0) / 60 for r in n], [r[col] * scale for r in n], color=COL[i], lw=1.4, label=f"node {i}")
    ax.set_title(label, fontsize=10, loc="left"); ax.set_xlabel("minute", fontsize=8); ax.set_ylim(bottom=0)
    ax.grid(color="#e4e3df", lw=0.8); [ax.spines[s].set_visible(False) for s in ("top", "right")]
axes.flat[0].legend(fontsize=8, frameon=False)
ax = axes.flat[5]
for ph, c in (("put", COL[0]), ("get", COL[1])):
    m = {}
    for r in ps:
        if r["phase"].lower() == ph: m.setdefault((int(r["unix_s"]) - pt0) // 60, []).append(int(r["ops"]))
    if not m: continue
    xs = sorted(m); ax.plot(xs, [sum(m[x]) / 60 for x in xs], color=c, lw=1.6, label=f"{ph.upper()} ops/s (background load)")
ax.set_title("Background load, ops/s per minute", fontsize=10, loc="left"); ax.set_xlabel("minute", fontsize=8); ax.set_ylim(bottom=0)
ax.grid(color="#e4e3df", lw=0.8); [ax.spines[s].set_visible(False) for s in ("top", "right")]
if ps: ax.legend(fontsize=8, frameon=False)
else: ax.set_axis_off(); ax.text(0.02, 0.5, "No per-second load trace:\ns3_bench writes it when a phase ends,\nand this run stopped first.", fontsize=10, color="#555", transform=ax.transAxes)
fig.suptitle(title, fontsize=11, x=0.01, ha="left"); fig.tight_layout(); fig.savefig(out); print(out)
