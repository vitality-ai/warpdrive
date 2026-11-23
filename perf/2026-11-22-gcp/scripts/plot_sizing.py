#!/usr/bin/env python3
"""plot_sizing.py <sizing dir> <out.png> <title>: RSS per node while loading (load_rss.csv), and idle RSS after each restart
(sizing.csv) against objects stored. Two charts, no shared axes."""
import csv, sys
import matplotlib; matplotlib.use("Agg"); import matplotlib.pyplot as plt
d, out, title = sys.argv[1:4]
NODE = ["#2a78d6", "#eb6834", "#1baf7a"]
def style(ax, t, xl):
    ax.set_title(t, fontsize=10, loc="left"); ax.set_xlabel(xl, fontsize=8); ax.set_ylim(bottom=0)
    ax.grid(color="#e4e3df", lw=0.8); [ax.spines[s].set_visible(False) for s in ("top", "right")]
lr = [r for r in csv.DictReader(open(f"{d}/load_rss.csv")) if r["rss_kb"]]
sz = list(csv.DictReader(open(f"{d}/sizing.csv")))
t0 = min(int(r["unix_s"]) for r in lr)
fig, axes = plt.subplots(1, 2, figsize=(13, 4.2), dpi=120)
for n in range(3):
    pts = [((int(r["unix_s"]) - t0) / 60, int(r["rss_kb"]) / 1048576) for r in lr if int(r["node"]) == n]
    axes[0].plot([p[0] for p in pts], [p[1] for p in pts], color=NODE[n], lw=1.4, label=f"node {n}" + (" (15 GB)" if n == 0 else " (8 GB)"))
    idle = [(int(r["objects_total"]) / 1e6, int(r["rss_kb_idle"]) / 1048576) for r in sz if int(r["node"]) == n]
    axes[1].plot([p[0] for p in idle], [p[1] for p in idle], color=NODE[n], lw=1.8, marker="o", ms=5, label=f"node {n}")
for ax in axes: ax.axhline(6, color="#888", lw=1, ls="--")
axes[0].text(0.3, 6.05, "6 GB guard", fontsize=7, color="#666")
style(axes[0], "Memory (RSS) while loading, GB", "minute of the run (sampled only while load runs)")
style(axes[1], "Idle memory after restart, GB", "million objects stored")
axes[0].legend(fontsize=8, frameon=False, loc="upper center", bbox_to_anchor=(0.5, 0.88), ncol=3)
fig.suptitle(title, fontsize=10, x=0.01, ha="left"); fig.tight_layout(); fig.savefig(out); print(out)
