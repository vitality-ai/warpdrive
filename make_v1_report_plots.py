"""
Generates the three plots for warpdrive-distributed/docs/benchmarks/v1.0.0-real-results.md
directly from the real numbers measured in this session (see each script's own
output, reproduced in the numbers below) -- not estimated or smoothed.

Run: python3 make_v1_report_plots.py
Writes PNGs into ../warpdrive-distributed/docs/benchmarks/images/
"""
import os

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt

OUT_DIR = "../warpdrive-distributed/docs/benchmarks/images"
os.makedirs(OUT_DIR, exist_ok=True)

BAR_PLAIN = "#9ca3af"
BAR_PACKED = "#ff8a3d"
GRID = "#e5e7eb"


def style_ax(ax):
    ax.spines["top"].set_visible(False)
    ax.spines["right"].set_visible(False)
    ax.grid(axis="y", color=GRID, linewidth=0.8, zorder=0)
    ax.set_axisbelow(True)


def savefig(fig, name):
    path = os.path.join(OUT_DIR, name)
    fig.savefig(path, dpi=160, bbox_inches="tight")
    print(f"wrote {path}")


# --- Plot 1: Lance data-fragment row-batch packing (20k vectors, 768-dim) ---
nprobes = [4, 16, 40]
plain_ms = [603.5, 604.9, 616.9]
packed_ms = [14.7, 29.8, 24.0]

fig, ax = plt.subplots(figsize=(6, 4))
x = range(len(nprobes))
w = 0.35
ax.bar([i - w / 2 for i in x], plain_ms, width=w, label="plain bucket", color=BAR_PLAIN, zorder=3)
ax.bar([i + w / 2 for i in x], packed_ms, width=w, label="content-dependent (row-batch packed)", color=BAR_PACKED, zorder=3)
ax.set_yscale("log")
ax.set_xticks(list(x))
ax.set_xticklabels([f"nprobe={n}" for n in nprobes])
ax.set_ylabel("median query latency (ms, log scale)")
ax.set_title("Lance IVF_PQ `take` latency: plain vs. row-batch-packed\n(20k vectors, 768-dim, real Lance dataset, RS(3,2))")
for i, (p, k) in enumerate(zip(plain_ms, packed_ms)):
    ax.annotate(f"{p/k:.0f}x", (i, max(p, k) * 1.15), ha="center", fontsize=10, fontweight="bold")
ax.set_ylim(top=ax.get_ylim()[1] * 2.2)
style_ax(ax)
ax.legend(frameon=False, loc="upper center")
savefig(fig, "lance_rowbatch_latency.png")
plt.close(fig)


# --- Plot 2: SIFT1M-small (real, externally-recognized benchmark) ---
nprobes2 = [5, 10, 20]
plain2 = [26.35, 6.08, 5.99]
packed2 = [6.68, 1.10, 1.24]
recall = [0.743, 0.765, 0.768]  # identical for both buckets at every nprobe

fig, (ax1, ax2) = plt.subplots(1, 2, figsize=(10, 4))
x = range(len(nprobes2))
w = 0.35
ax1.bar([i - w / 2 for i in x], plain2, width=w, label="plain bucket", color=BAR_PLAIN, zorder=3)
ax1.bar([i + w / 2 for i in x], packed2, width=w, label="packed (IvfCentroidPacker)", color=BAR_PACKED, zorder=3)
ax1.set_xticks(list(x))
ax1.set_xticklabels([f"nprobe={n}" for n in nprobes2])
ax1.set_ylabel("median query latency (ms)")
ax1.set_title("Latency")
style_ax(ax1)
ax1.legend(frameon=False, loc="upper right")

ax2.plot(nprobes2, recall, marker="o", color="#4a0e63", linewidth=2, zorder=3, label="recall@10 (plain == packed, exact)")
ax2.set_ylim(0.6, 0.85)
ax2.set_xticks(nprobes2)
ax2.set_xlabel("nprobe")
ax2.set_ylabel("recall@10")
ax2.set_title("Accuracy (identical in both buckets)")
style_ax(ax2)
ax2.legend(frameon=False, loc="lower right")

fig.suptitle("SIFT1M-small (texmex corpus, real published benchmark dataset)", y=1.03, fontsize=12, fontweight="bold")
savefig(fig, "sift1m_small_latency_recall.png")
plt.close(fig)


# --- Plot 3: TPC-H SF1 official Q6 (pushdown-benefiting) vs Q1 (control) ---
fig, ax = plt.subplots(figsize=(6, 4))
labels = ["Q6 plain\n(selective)", "Q6 packed\n(selective)", "Q1 packed\n(low selectivity, control)"]
values = [7756.6, 51.9, 419.7]
colors = [BAR_PLAIN, BAR_PACKED, "#c92a6b"]
bars = ax.bar(labels, values, color=colors, zorder=3)
ax.set_yscale("log")
ax.set_ylabel("median query latency (ms, log scale)")
ax.set_title("Official TPC-H SF1 (6M rows), real dbgen data\nsorted by l_shipdate for row-group pruning")
ax.annotate("149x", (0.5, 350), ha="center", fontsize=12, fontweight="bold")
style_ax(ax)
savefig(fig, "tpch_pushdown_latency.png")
plt.close(fig)

print("done.")
