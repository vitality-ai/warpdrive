#!/usr/bin/env python3
"""plot_sizes.py <raw dir> <out.png>: PUT and GET throughput (MB/s) and request rate against object size, one line per arm.
Four charts, no shared axes."""
import glob, os, sys
import matplotlib; matplotlib.use("Agg"); import matplotlib.pyplot as plt
d, out = sys.argv[1:3]
ARMS = {"bitcask": ("Bitcask", "#eb6834"), "combined": ("RocksDB + #238", "#2a78d6")}
fig, axes = plt.subplots(1, 4, figsize=(17, 4), dpi=120)
for arm, (label, col) in ARMS.items():
    pts = []
    for f in glob.glob(f"{d}/{arm}-*.csv"):
        size = os.path.basename(f)[len(arm) + 1:-4]
        if not size.isdigit(): continue
        rows = {l.split(",")[0]: l.split(",") for l in open(f) if l.strip()}
        pts.append((int(size), float(rows["put"][5]), float(rows["get"][5])))
    pts.sort()
    xs = [p[0] / 1024 for p in pts]
    for ax, ys in zip(axes, ([p[1] * p[0] / 1e6 for p in pts], [p[2] * p[0] / 1e6 for p in pts], [p[1] for p in pts], [p[2] for p in pts])):
        ax.plot(xs, ys, color=col, marker="o", ms=5, lw=1.8, label=label)
for ax, t in zip(axes, ("PUT, MB/s", "GET, MB/s", "PUTs per second", "GETs per second")):
    ax.set_xscale("log"); ax.set_title(t, fontsize=10, loc="left"); ax.set_xlabel("object size, KB (log)", fontsize=8)
    if "per second" in t: ax.set_yscale("log")
    else: ax.set_ylim(bottom=0)
    ax.grid(color="#e4e3df", lw=0.8); [ax.spines[s].set_visible(False) for s in ("top", "right")]
axes[0].legend(fontsize=8, frameon=False)
fig.suptitle("RS(3,2), 5 x n2-standard-2, 60 s per phase; clients 64 (4 KB, 64 KB), 32 (1 MB), 16 (8 MB), 4 (64 MB)", fontsize=10, x=0.01, ha="left")
fig.tight_layout(); fig.savefig(out); print(out)
