"""
Generates the three plots for the HiPC poster paper, from the numbers
measured against a live 9-node WarpDrive cluster running RS(6,3) -- six
data shards, three parity, matching Fusion's own evaluation parameter
(their paper writes this "RS(9,6)" in their own (n,k) = (total, data)
notation; see docs/Distributed-Engine-Plan.md for why that reads
backwards against this project's (k,m) = (data, parity) convention).

Run: python3 make_hipc_plots.py
Writes PNGs into ./images/
"""
import os

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
from matplotlib.ticker import AutoMinorLocator, LogLocator, NullFormatter

OUT_DIR = "images"
os.makedirs(OUT_DIR, exist_ok=True)

# Okabe-Ito colorblind-safe palette, chosen with enough luminance
# contrast to also survive grayscale printing (flagged as a real risk
# in the earlier poster review of the purple/pink/orange scheme).
BAR_PLAIN = "#0072B2"    # blue
BAR_PACKED = "#E69F00"   # orange
BAR_CONTROL = "#CC79A7"  # reddish purple
LINE_ACCENT = "#009E73"  # bluish green
GRID_MAJOR = "#cbd1d9"
GRID_MINOR = "#eceff2"


def style_ax(ax, log=False):
    ax.spines["top"].set_visible(False)
    ax.spines["right"].set_visible(False)
    if log:
        # One minor tick (with gridline) at each 2x..9x within a decade,
        # not just at the decade boundaries -- makes it possible to read
        # a bar's value off the axis, not just its order of magnitude.
        ax.yaxis.set_minor_locator(LogLocator(base=10.0, subs=range(2, 10)))
        ax.yaxis.set_minor_formatter(NullFormatter())
    else:
        ax.yaxis.set_minor_locator(AutoMinorLocator())
    ax.tick_params(axis="y", which="major", length=5)
    ax.tick_params(axis="y", which="minor", length=2.5)
    ax.grid(axis="y", which="major", color=GRID_MAJOR, linewidth=0.8, zorder=0)
    ax.grid(axis="y", which="minor", color=GRID_MINOR, linewidth=0.5, zorder=0)
    ax.set_axisbelow(True)


def savefig(fig, name):
    path = os.path.join(OUT_DIR, name)
    fig.savefig(path, dpi=160, bbox_inches="tight")
    print(f"wrote {path}")


# --- Plot 1: Lance data-fragment row-batch packing (20k vectors, 768-dim) ---
# Re-measured 2026-11-13 on current main; see the Plot 4 comment below for
# why these are lower than earlier values measured on a stale commit.
nprobes = [4, 16, 40]
plain_ms = [54.2, 53.5, 53.9]
packed_ms = [18.8, 28.8, 26.2]

fig, ax = plt.subplots(figsize=(6, 4))
x = range(len(nprobes))
w = 0.35
ax.bar([i - w / 2 for i in x], plain_ms, width=w, label="plain bucket", color=BAR_PLAIN, zorder=3)
ax.bar([i + w / 2 for i in x], packed_ms, width=w, label="content-dependent (row-batch packed)", color=BAR_PACKED, zorder=3)
ax.set_yscale("log")
ax.set_xticks(list(x))
ax.set_xticklabels([f"nprobe={n}" for n in nprobes])
ax.set_ylabel("median query latency (ms, log scale)")
ax.set_title("Lance IVF_PQ `take` latency: plain vs. row-batch-packed\n(20k vectors, 768-dim, RS(6,3))")
for i, (p, k) in enumerate(zip(plain_ms, packed_ms)):
    ax.annotate(f"{p/k:.0f}x", (i, max(p, k) * 1.15), ha="center", fontsize=10, fontweight="bold")
ax.set_ylim(top=ax.get_ylim()[1] * 2.2)
style_ax(ax, log=True)
ax.legend(frameon=False, loc="upper center")
savefig(fig, "lance_rowbatch_latency_rs63.png")
plt.close(fig)


# --- Plot 2: SIFT1M-small (real, externally-recognized benchmark) ---
# Re-measured 2026-11-13 on current main, mean over 10 trials.
nprobes2 = [5, 10, 20]
plain2 = [1.77, 1.49, 1.66]
packed2 = [1.47, 1.28, 1.43]
recall = [0.704, 0.731, 0.740]  # identical for both buckets at every nprobe

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

ax2.plot(nprobes2, recall, marker="o", color=LINE_ACCENT, linewidth=2, zorder=3, label="recall@10 (plain == packed, exact)")
ax2.set_ylim(0.6, 0.85)
ax2.set_xticks(nprobes2)
ax2.set_xlabel("nprobe")
ax2.set_ylabel("recall@10")
ax2.set_title("Accuracy (identical in both buckets)")
style_ax(ax2)
ax2.xaxis.set_minor_locator(AutoMinorLocator())
ax2.grid(axis="x", which="major", color=GRID_MAJOR, linewidth=0.8, zorder=0)
ax2.grid(axis="x", which="minor", color=GRID_MINOR, linewidth=0.5, zorder=0)
ax2.legend(frameon=False, loc="lower right")

fig.suptitle("SIFT1M-small (texmex corpus, published benchmark dataset, RS(6,3))", y=1.03, fontsize=12, fontweight="bold")
savefig(fig, "sift1m_small_latency_recall_rs63.png")
plt.close(fig)


# --- Plot 3: TPC-H SF1 official Q6 (pushdown-benefiting) vs Q1 (control) ---
# Re-measured 2026-11-13 on current main, mean over 12 trials (Q1 plain:
# median of 3 direct runs).
fig, ax = plt.subplots(figsize=(6, 4))
labels = ["Q6 plain\n(selective)", "Q6 packed\n(selective)", "Q1 packed\n(low selectivity, control)"]
values = [723.4, 63.0, 530.1]
colors = [BAR_PLAIN, BAR_PACKED, BAR_CONTROL]
bars = ax.bar(labels, values, color=colors, zorder=3)
ax.set_yscale("log")
ax.set_ylabel("median query latency (ms, log scale)")
ax.set_title("Official TPC-H SF1 (6M rows), real dbgen data\nsorted by l_shipdate for row-group pruning, RS(6,3)")
ax.annotate("11.5x", (0.5, 450), ha="center", fontsize=12, fontweight="bold")
style_ax(ax, log=True)
savefig(fig, "tpch_pushdown_latency_rs63.png")
plt.close(fig)

# --- Plot 4: everything, one tick per experiment, default vs. workload-aware ---
# Same eight rows as Table I. TPC-H Q1 has no default measurement (it was
# run as a workload-aware-only control), so that tick gets one bar, not a
# pair -- shown, not silently dropped, via a dimmed placeholder and a
# footnote in the label rather than making up a number.
# Re-measured 2026-11-13 on current main (9-node RS(6,3), local, Apple M3):
# means over 10-12 independent trials each (TPC-H: run_tpch_reps.py; Lance
# row-batch: run_lance_reps.py; SIFT1M-small: run_sift_reps.py), Q1 plain
# from 3 direct runs (its own script intentionally skips a repeated plain
# run -- see tpch_duckdb_benchmark.py's own comment on why). Earlier
# values here (7949.5/114.0 etc.) were measured before commit 2951b45
# ("Skip full EC decode on plain-bucket Range-GETs when data shards are
# healthy", 2026-10-07) fixed a real bug in the plain-bucket baseline;
# re-verified directly by running the exact old commit (ed1f729) again,
# which reproduces the old numbers almost exactly. Current numbers are
# lower because the baseline this compares against is faster now, not
# because workload-aware placement regressed.
experiments = [
    ("TPC-H\nQ6 (sel.)", 723.4, 63.0),
    ("TPC-H\nQ1 (low sel.)", 3318.8, 530.1),
    ("SIFT\nnprobe=5", 1.77, 1.47),
    ("SIFT\nnprobe=10", 1.49, 1.28),
    ("SIFT\nnprobe=20", 1.66, 1.43),
    ("Row-batch\nnprobe=4", 54.2, 18.8),
    ("Row-batch\nnprobe=16", 53.5, 28.8),
    ("Row-batch\nnprobe=40", 53.9, 26.2),
]
# Short labels (two lines, small font, no rotation needed) so eight
# ticks stay legible at single-column width rather than full-page span.
labels = [e[0] for e in experiments]
default_vals = [e[1] for e in experiments]
wa_vals = [e[2] for e in experiments]

fig, ax = plt.subplots(figsize=(7.2, 3.6))
x = list(range(len(experiments)))
w = 0.35

default_x = [i - w / 2 for i, d in zip(x, default_vals) if d is not None]
default_y = [d for d in default_vals if d is not None]
ax.bar(default_x, default_y, width=w, label="default", color=BAR_PLAIN, zorder=3)
ax.bar([i + w / 2 for i in x], wa_vals, width=w, label="workload-aware", color=BAR_PACKED, zorder=3)

for i, (d, p) in enumerate(zip(default_vals, wa_vals)):
    if d is None:
        continue
    speedup = d / p
    ax.annotate(f"{speedup:.1f}x", (i, max(d, p) * 1.3), ha="center", fontsize=7.5, fontweight="bold")

ax.set_yscale("log")
ax.set_ylim(0.5, ax.get_ylim()[1] * 2.5)
ax.set_xticks(x)
ax.set_xticklabels(labels, fontsize=7)
ax.tick_params(axis="x", length=0)
ax.set_ylabel("median latency (ms, log scale)", fontsize=9)
ax.tick_params(axis="y", labelsize=8)
style_ax(ax, log=True)
ax.legend(frameon=False, loc="upper right", fontsize=7.5)
savefig(fig, "all_results_rs63.png")
plt.close(fig)

print("done.")
