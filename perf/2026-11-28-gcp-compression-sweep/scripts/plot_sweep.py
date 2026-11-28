#!/usr/bin/env python3
"""plot_sweep.py <sweep dir>: from means.csv, zstd and auto ops/s relative to off per size (text and random, PUT
and GET), and 1 MB text ops/s against client count. Writes sweep.png and clients.png."""
import csv, os, sys
import matplotlib; matplotlib.use("Agg"); import matplotlib.pyplot as plt
d = sys.argv[1]; m = list(csv.DictReader(open(os.path.join(d, "means.csv"))))
COLOR = {"off": "#8a8f98", "zstd": "#2a78d6", "auto": "#eb6834"}; INK, MUTED, GRID = "#1f2328", "#5b636e", "#e3e6ea"
def style(ax):
    for s in ("top", "right"): ax.spines[s].set_visible(False)
    for s in ("left", "bottom"): ax.spines[s].set_color(GRID)
    ax.tick_params(colors=MUTED); ax.grid(axis="y", color=GRID, lw=0.8); ax.set_axisbelow(True)
sizes = [4096, 65536, 1048576, 8388608]; names = ["4 KB", "64 KB", "1 MB", "8 MB"]
fig, axes = plt.subplots(2, 2, figsize=(11, 7), sharey=True)
for (r, data), (c, ph) in [((r, dt), (c, p)) for r, dt in enumerate(["text", "random"]) for c, p in enumerate(["put", "get"])]:
    ax = axes[r][c]; style(ax)
    v = {(x["arm"], int(x["size"])): float(x["ops_per_s_mean"]) for x in m if x["stage"] == "sweep" and x["data"] == data and x["phase"] == ph}
    for j, arm in enumerate(["zstd", "auto"]):
        ys = [100 * (v[(arm, s)] / v[("off", s)] - 1) for s in sizes]
        xs = [i + (j - 0.5) * 0.36 for i in range(4)]
        ax.bar(xs, ys, 0.34, color=COLOR[arm], label=arm, edgecolor="white", lw=2)
        for x, y in zip(xs, ys): ax.text(x, y + (1 if y >= 0 else -1), f"{y:+.0f}%", ha="center", va="bottom" if y >= 0 else "top", fontsize=8, color=INK)
    ax.axhline(0, color=MUTED, lw=1); ax.set_xticks(range(4), names)
    ax.set_title(f"{data} data, {ph.upper()}", color=INK, fontsize=11, loc="left")
    if c == 0: ax.set_ylabel("ops/s vs off (%)", color=MUTED)
axes[0][0].legend(frameon=False, loc="lower left")
fig.suptitle("Compression against off, 5 x n2-standard-2, RS(3,2), mean of 3 runs", color=INK, x=0.01, ha="left")
fig.tight_layout(); fig.savefig(os.path.join(d, "sweep.png"), dpi=130)
fig, axes = plt.subplots(1, 2, figsize=(11, 4))
for ax, ph in zip(axes, ["put", "get"]):
    style(ax)
    for arm in ["off", "zstd", "auto"]:
        pts = sorted((int(x["clients"]), float(x["ops_per_s_mean"])) for x in m if x["stage"] == "clients" and x["arm"] == arm and x["phase"] == ph)
        ax.plot([p[0] for p in pts], [p[1] for p in pts], color=COLOR[arm], lw=2, marker="o", ms=8, label=arm)
    ax.set_xscale("log", base=2); ax.set_xticks([8, 16, 32, 64, 128], ["8", "16", "32", "64", "128"]); ax.set_ylim(0, None)
    ax.set_xlabel("clients", color=MUTED); ax.set_ylabel("ops/s", color=MUTED); ax.set_title(f"1 MB text, {ph.upper()}", color=INK, loc="left")
axes[1].legend(frameon=False, loc="lower right")
fig.tight_layout(); fig.savefig(os.path.join(d, "clients.png"), dpi=130)
