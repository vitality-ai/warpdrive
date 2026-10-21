#!/usr/bin/env python3
"""Generates the plots for docs/durability-and-minio-comparison.md.

Data is transcribed from docs/PERF-JOURNAL.md's 2026-10-21 entries. Run from
this directory: python3 gen_plots.py
"""
import matplotlib.pyplot as plt
import numpy as np

WARPDRIVE = "#2a78d6"
MINIO = "#eb6834"

plt.rcParams.update({
    "font.size": 11,
    "axes.edgecolor": "#888888",
    "axes.labelcolor": "#333333",
    "text.color": "#222222",
    "xtick.color": "#333333",
    "ytick.color": "#333333",
    "axes.spines.top": False,
    "axes.spines.right": False,
    "figure.facecolor": "white",
    "axes.facecolor": "white",
})


def bar_with_labels(ax, x, heights, width, color, label):
    bars = ax.bar(x, heights, width, color=color, label=label)
    for b in bars:
        h = b.get_height()
        ax.annotate(f"{h:.0f}", (b.get_x() + b.get_width() / 2, h),
                    xytext=(0, 3), textcoords="offset points",
                    ha="center", va="bottom", fontsize=9)
    return bars


# ---- Chart 1: durability modes, single coordinator, old hardware ----
fig, ax = plt.subplots(figsize=(6.5, 4))
modes = ["No fsync\n(not durable)", "Naive fsync", "Group-commit\n(shipped)"]
c64 = [811.9, 137.1, 241.4]
c256 = [834.7, 154.1, 251.7]
x = np.arange(len(modes))
w = 0.35
bar_with_labels(ax, x - w / 2, c64, w, WARPDRIVE, "concurrency=64")
bar_with_labels(ax, x + w / 2, c256, w, "#1baf7a", "concurrency=256")
ax.set_xticks(x)
ax.set_xticklabels(modes)
ax.set_ylabel("req/s")
ax.set_title("Single coordinator throughput by write-durability mode\n(e2-small / pd-standard, RS(3,2), 5-node cluster)")
ax.legend(frameon=False)
fig.tight_layout()
fig.savefig("durability_modes.png", dpi=150)
plt.close(fig)


# ---- Chart 2: hardware upgrade, 5-node aggregate, log scale ----
fig, ax = plt.subplots(figsize=(6.5, 4))
groups = ["MinIO", "WarpDrive"]
old_hw = [380.2, 837.4]
new_hw = [1601.7, 4742.8]
x = np.arange(len(groups))
w = 0.35
bar_with_labels(ax, x - w / 2, old_hw, w, "#9a9a9a", "old hw: e2-small / pd-standard")
bar_with_labels(ax, x + w / 2, new_hw, w, WARPDRIVE, "new hw: n2-standard-2 / local SSD")
ax.set_xticks(x)
ax.set_xticklabels(groups)
ax.set_yscale("log")
ax.set_ylabel("req/s or obj/s (log scale)")
ax.set_title("5-node aggregate throughput, old vs. new hardware\n(each system compared only to itself, same tool per system)")
ax.legend(frameon=False)
fig.tight_layout()
fig.savefig("hardware_upgrade.png", dpi=150)
plt.close(fig)


# ---- Chart 3: same client harness, new hardware ----
fig, axes = plt.subplots(1, 2, figsize=(9, 4))

ax = axes[0]
labels = ["concurrency=64", "concurrency=256"]
minio_vals = [672.7, 752.0]
wd_vals = [1160.2, 1110.3]
x = np.arange(len(labels))
w = 0.35
bar_with_labels(ax, x - w / 2, minio_vals, w, MINIO, "MinIO")
bar_with_labels(ax, x + w / 2, wd_vals, w, WARPDRIVE, "WarpDrive")
ax.set_xticks(x)
ax.set_xticklabels(labels)
ax.set_ylabel("obj/s or req/s")
ax.set_title("Single node, direct")
ax.set_ylim(0, 1400)
ax.legend(frameon=False, loc="upper center")

ax = axes[1]
labels = ["5-node spread\n(128 concurrency each)"]
x = np.arange(len(labels))
bar_with_labels(ax, x - w / 2, [1513.7], w, MINIO, "MinIO")
bar_with_labels(ax, x + w / 2, [4544.0], w, WARPDRIVE, "WarpDrive")
ax.set_xticks(x)
ax.set_xticklabels(labels)
ax.set_ylabel("aggregate obj/s or req/s")
ax.set_title("5-node cluster")
ax.legend(frameon=False)

fig.suptitle("Same patched `warp` binary against both servers, new hardware")
fig.tight_layout()
fig.savefig("same_harness_comparison.png", dpi=150)
plt.close(fig)

print("wrote durability_modes.png, hardware_upgrade.png, same_harness_comparison.png")
