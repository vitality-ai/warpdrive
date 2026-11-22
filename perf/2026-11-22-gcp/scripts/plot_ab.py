#!/usr/bin/env python3
"""plot_ab.py <summary.csv> <out.png> <title> <arm|arm|...>: PUT/s and GET/s per setup, one bar per arm
(mean of that arm's runs, value printed on each bar), as two charts side by side."""
import csv, sys, statistics as st
import matplotlib; matplotlib.use("Agg"); import matplotlib.pyplot as plt
src, out, title, arms = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4].split("|")
COL = ["#2a78d6", "#eb6834", "#1baf7a", "#eda100"]
rows = list(csv.DictReader(open(src)))
setups = [s for s in ["RS(2,1) 4 KB", "RS(2,1) 1 MB", "Single node 4 KB", "Single node 1 MB"] if any(r["setup"] == s for r in rows)]
fig, axes = plt.subplots(1, 2, figsize=(12, 4.4), dpi=130)
for ax, (field, label) in zip(axes, [("put_ops_s", "PUT ops/s"), ("get_ops_s", "GET ops/s")]):
    w = 0.8 / len(arms)
    for i, a in enumerate(arms):
        vals = [st.mean([float(r[field]) for r in rows if r["setup"] == s and r["arm"] == a] or [0]) for s in setups]
        xs = [j + (i - (len(arms) - 1) / 2) * w for j in range(len(setups))]
        bars = ax.bar(xs, vals, w * 0.92, color=COL[i], label=a)
        ax.bar_label(bars, labels=[f"{v:,.0f}" for v in vals], fontsize=7, padding=2)
    ax.set_xticks(range(len(setups)), setups, fontsize=8); ax.set_title(label, fontsize=10, loc="left")
    ax.grid(axis="y", color="#e4e3df", lw=0.8); ax.set_axisbelow(True)
    for s in ("top", "right"): ax.spines[s].set_visible(False)
    ax.yaxis.set_major_formatter(matplotlib.ticker.FuncFormatter(lambda v, _: f"{v:,.0f}"))
axes[0].legend(fontsize=8, frameon=False, loc="upper right")
fig.suptitle(title, fontsize=11, x=0.01, ha="left"); fig.tight_layout(); fig.savefig(out); print(out)
