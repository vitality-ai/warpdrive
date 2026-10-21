#!/usr/bin/env python3
"""Generates the plots for docs/durability-and-minio-comparison.md and the
v1.0.0-beta release notes.

Data is transcribed from docs/PERF-JOURNAL.md's 2026-10-21 entries. Run from
this directory: python3 gen_plots.py
"""
import matplotlib.pyplot as plt
import matplotlib.ticker as mticker
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
    bars = ax.bar(x, heights, width, color=color, label=label, zorder=3)
    for b in bars:
        h = b.get_height()
        ax.annotate(f"{h:.0f}", (b.get_x() + b.get_width() / 2, h),
                    xytext=(0, 3), textcoords="offset points",
                    ha="center", va="bottom", fontsize=9, zorder=4)
    return bars


def style_grid(ax, log=False):
    """Major + minor gridlines and ticks on the y-axis, bars drawn on top."""
    ax.set_axisbelow(True)
    ax.minorticks_on()
    ax.tick_params(axis="x", which="minor", bottom=False)
    if log:
        ax.yaxis.set_minor_locator(mticker.LogLocator(base=10.0, subs=np.arange(2, 10), numticks=12))
        ax.yaxis.set_minor_formatter(mticker.NullFormatter())
    ax.grid(which="major", axis="y", color="#c9c9c9", linewidth=0.8, zorder=0)
    ax.grid(which="minor", axis="y", color="#dadada", linewidth=0.6, zorder=0)


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
style_grid(ax)
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
style_grid(ax, log=True)
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
style_grid(ax)

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
style_grid(ax)

fig.suptitle("Same patched `warp` binary against both servers, new hardware")
fig.tight_layout()
fig.savefig("same_harness_comparison.png", dpi=150)
plt.close(fig)


# ---- Chart 4: the complete story -- three stages of increasing rigor,
# same ~2-3x result holding up at every stage, not a methodology artifact.
# Stage 1: old hardware, mismatched client tools (the original comparison).
# Stage 2: new hardware, same (still mismatched) tools -- isolates what the
#          hardware upgrade alone did.
# Stage 3: new hardware, one patched client driving both servers -- the
#          final, methodology-clean number.
# All three stages use durable (fsync'd) writes on both systems throughout;
# only disk/hardware and client-harness rigor change across stages, not
# durability.
fig, ax = plt.subplots(figsize=(8, 5))
stages = ["Old hardware\n(mismatched tools)", "New hardware\n(mismatched tools)", "New hardware\n(same client tool)"]
minio_vals = [380.2, 1601.7, 1513.7]
wd_vals = [837.4, 4742.8, 4544.0]
x = np.arange(len(stages))
w = 0.32
bar_with_labels(ax, x - w / 2, minio_vals, w, MINIO, "MinIO, 5-node aggregate")
bar_with_labels(ax, x + w / 2, wd_vals, w, WARPDRIVE, "WarpDrive, 5-node aggregate")
ax.set_xticks(x)
ax.set_xticklabels(stages)
ax.set_yscale("log")
ax.set_ylim(top=max(wd_vals) * 2.2)
for i, (m, wd) in enumerate(zip(minio_vals, wd_vals)):
    ax.annotate(f"{wd / m:.1f}x", (x[i], max(m, wd) * 1.45), ha="center", fontsize=11, fontweight="bold", color="#333333")
ax.set_ylabel("aggregate req/s or obj/s (log scale)")
ax.set_title("The complete story: durability intact, hardware fixed, methodology fixed\nWarpDrive's lead over MinIO holds at every stage of added rigor")
ax.legend(frameon=False, loc="upper left")
style_grid(ax, log=True)
fig.tight_layout()
fig.savefig("complete_story.png", dpi=150)
plt.close(fig)


print("wrote durability_modes.png, hardware_upgrade.png, same_harness_comparison.png, complete_story.png")
