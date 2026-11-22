#!/usr/bin/env python3
"""plot_comp.py <timeseries.csv> <run> <out prefix>: for each object size in the run, disk per node over time
(dashed line = live data) and PUT ops/s during the overwrite phase, one line per arm."""
import csv, sys
import matplotlib; matplotlib.use("Agg"); import matplotlib.pyplot as plt
src, run, prefix = sys.argv[1], sys.argv[2], sys.argv[3]
COL = ["#2a78d6", "#eb6834", "#1baf7a", "#eda100"]
rows = [r for r in csv.DictReader(open(src)) if r["run"] == run]
for size in dict.fromkeys(r["size"] for r in rows):
    sr = [r for r in rows if r["size"] == size]; arms = list(dict.fromkeys(r["arm"] for r in sr))
    # Same window for every arm: an arm whose collection hung has samples past the run.
    end = min(max(int(r["minute"]) for r in sr if r["arm"] == a) for a in arms)
    sr = [r for r in sr if int(r["minute"]) <= end]
    fig, axes = plt.subplots(1, 2, figsize=(12, 4.2), dpi=130)
    for ax, (field, label, put_only) in zip(axes, [("disk_gb", "Disk per node, GB (mean of nodes)", False), ("put_ops_s", "PUT ops/s, overwrite phase", True)]):
        for i, a in enumerate(arms):
            pts = [(int(r["minute"]), float(r[field])) for r in sr if r["arm"] == a and r[field] not in ("", None)]
            ax.plot([p[0] for p in pts], [p[1] for p in pts], color=COL[i % 4], lw=2, label=a)
        if field == "disk_gb" and sr:
            ax.axhline(float(sr[0]["live_gb"]), color="#8a8984", ls="--", lw=1.2, label="live data per node")
        ax.set_title(label, fontsize=10, loc="left"); ax.set_xlabel("minute", fontsize=8)
        ax.grid(color="#e4e3df", lw=0.8); ax.set_axisbelow(True); ax.set_ylim(bottom=0)
        for s in ("top", "right"): ax.spines[s].set_visible(False)
    axes[0].legend(fontsize=8, frameon=False)
    fig.suptitle(f"{size} objects: {run}", fontsize=11, x=0.01, ha="left"); fig.tight_layout()
    out = f"{prefix}-{size.replace(' ', '')}.png"; fig.savefig(out); print(out)
