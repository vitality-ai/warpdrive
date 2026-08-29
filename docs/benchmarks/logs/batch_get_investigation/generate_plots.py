#!/usr/bin/env python3
"""
Generates the Experiment 2 (batch-GET primitive investigation) plot set.

Parses the raw sample arrays directly out of the committed results .txt
files (rather than re-typing medians/ranges), and cross-reads Experiment
1's AnyBlob CSVs for the matched-connection-count comparison line, so
nothing here is a hand-transcribed number.

Usage:
    .venv/bin/python3 docs/benchmarks/logs/batch_get_investigation/generate_plots.py
"""
import csv
import re
import statistics
from pathlib import Path

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
import matplotlib.ticker as mticker

HERE = Path(__file__).resolve().parent
ANYBLOB_DIR = HERE.parent / "anyblob_baseline"
OUT = HERE / "plots"
OUT.mkdir(exist_ok=True)

NET_CEILING_1CONN = 1362.0
NET_CEILING_8CONN = 1985.0

BATCH_COLOR = "#1b9e77"       # same WarpDrive green as Experiment 1
SEQUENTIAL_COLOR = "#7570b3"  # distinct purple for "naive sequential"
ANYBLOB_COLOR = "#d95f02"     # same "other engineered client" orange as Experiment 1
CEILING_COLOR = "#555555"


def parse_samples(path, pattern):
    """Extracts the `samples=[...]` list following each line matching pattern."""
    text = Path(path).read_text()
    blocks = re.findall(pattern + r".*?samples=\s*\[([^\]]+)\]", text, re.S)
    return [[float(x) for x in b.split(",")] for b in blocks]


def style_axis(ax, xvals):
    ax.set_xscale("log", base=2)
    ax.set_xticks(xvals)
    ax.xaxis.set_major_formatter(mticker.ScalarFormatter())
    ax.xaxis.set_minor_formatter(mticker.NullFormatter())
    ax.minorticks_on()
    ax.yaxis.set_minor_locator(mticker.AutoMinorLocator())
    ax.grid(True, which="major", axis="both", linestyle="-", linewidth=0.8,
            color="#888888", alpha=0.85, zorder=0)
    ax.grid(True, which="minor", axis="both", linestyle=":", linewidth=0.6,
            color="#aaaaaa", alpha=0.6, zorder=0)
    ax.set_axisbelow(True)


def read_anyblob_get(target):
    rows = []
    with open(ANYBLOB_DIR / f"{target}_get.csv.summary") as f:
        for r in csv.DictReader(f):
            c = int(r["Concurrency"])
            t = float(r["Time"])
            d = int(r["Datasize"])
            rows.append((c, (d / 1e6) / (t / 1000.0)))
    rows.sort()
    return rows


def main():
    single_conn_file = HERE / "single_conn_tightened_results.txt"
    batch_samples = parse_samples(single_conn_file, r"batch-GET, 256 objects \(2GB\), 1 connection ===\n")[0]
    seq_samples = parse_samples(single_conn_file, r"sequential 256x8MiB individual GETs, 1 connection ===\n")[0]

    sweep_file = HERE / "parallel_full_sweep_results.txt"
    sweep_text = sweep_file.read_text()
    sweep = {}
    for m in re.finditer(r"nhints=\s*(\d+).*?samples=\[([^\]]+)\]", sweep_text):
        n = int(m.group(1))
        vals = [float(x) for x in m.group(2).split(",")]
        sweep[n] = vals
    nhints_sorted = sorted(sweep.keys())

    # AnyBlob individual GET at matched total-connection-count (t=4 fixed,
    # so total connections = 4*c) -- read straight from Experiment 1's CSV.
    wd_get = dict(read_anyblob_get("warpdrive"))
    matched = {4 * c: tp for c, tp in wd_get.items()}  # total_conn -> MB/s

    # ------------------------------------------------------------------
    # Figure 1: single-connection comparison -- bar chart with min/max
    # whiskers, the headline "is batching competitive with naive
    # sequential at the same connection count" result.
    # ------------------------------------------------------------------
    fig1, ax = plt.subplots(figsize=(7.5, 6))
    labels = ["Sequential\n256x individual GETs", "Batch-GET\n256 objects, 1 request"]
    meds = [statistics.median(seq_samples), statistics.median(batch_samples)]
    mins = [min(seq_samples), min(batch_samples)]
    maxs = [max(seq_samples), max(batch_samples)]
    colors = [SEQUENTIAL_COLOR, BATCH_COLOR]
    yerr = [[m - lo for m, lo in zip(meds, mins)], [hi - m for m, hi in zip(meds, maxs)]]

    bars = ax.bar(labels, meds, color=colors, width=0.55, zorder=3,
                   edgecolor="black", linewidth=0.6)
    ax.errorbar(labels, meds, yerr=yerr, fmt="none", ecolor="black",
                capsize=8, linewidth=1.4, zorder=4)
    for i, (bar, med, n) in enumerate(zip(bars, meds, [len(seq_samples), len(batch_samples)])):
        ax.annotate(f"median {med:.1f} MB/s\n(n={n})", (bar.get_x() + bar.get_width() / 2, maxs[i]),
                    textcoords="offset points", xytext=(0, 10), ha="center", fontsize=9)
    gap_pct = 100.0 * (meds[0] - meds[1]) / meds[0]
    ax.annotate(f"gap: {gap_pct:.1f}%\n(1 round trip vs 256)",
                (0.5, max(maxs) * 0.55), xycoords=("axes fraction", "data"),
                ha="center", fontsize=10, fontweight="bold",
                bbox=dict(boxstyle="round,pad=0.4", fc="#fff8e1", ec="#999999"))
    ax.set_ylim(0, max(maxs) * 1.28)
    ax.set_ylabel("Throughput (MB/s)")
    ax.set_title("Experiment 2 — single-connection comparison\n"
                  "256 objects / 2GB, same connection, whiskers = min–max of 10 runs",
                  fontsize=11, fontweight="bold")
    ax.grid(True, axis="y", which="major", linestyle="-", linewidth=0.8, color="#888888", alpha=0.85, zorder=0)
    ax.grid(True, axis="y", which="minor", linestyle=":", linewidth=0.6, color="#aaaaaa", alpha=0.6, zorder=0)
    ax.yaxis.set_minor_locator(mticker.AutoMinorLocator())
    ax.set_axisbelow(True)
    fig1.tight_layout()
    fig1.savefig(OUT / "01_single_connection_comparison.png", dpi=150)
    plt.close(fig1)

    # ------------------------------------------------------------------
    # Figure 2: parallel scaling story -- two stacked panels.
    #  A. throughput vs threads, with AnyBlob's matched-connection-count
    #     curve overlaid, plus network ceiling references.
    #  B. per-thread-count spread (min/median/max) to make the "genuine
    #     plateau, not noise" finding visually explicit.
    # ------------------------------------------------------------------
    fig2, axes = plt.subplots(2, 1, figsize=(9.5, 10.5), sharex=True)
    fig2.suptitle("Experiment 2 — combining batching with modest parallelism\n"
                  "(plain ThreadPoolExecutor, no io_uring; 2GB total data held constant)",
                  fontsize=13, fontweight="bold")

    ax = axes[0]
    meds_sweep = [statistics.median(sweep[n]) for n in nhints_sorted]
    mins_sweep = [min(sweep[n]) for n in nhints_sorted]
    maxs_sweep = [max(sweep[n]) for n in nhints_sorted]
    yerr_sweep = [[m - lo for m, lo in zip(meds_sweep, mins_sweep)],
                  [hi - m for m, hi in zip(meds_sweep, maxs_sweep)]]
    ax.errorbar(nhints_sorted, meds_sweep, yerr=yerr_sweep, fmt="s-", color=BATCH_COLOR,
                linewidth=2, markersize=7, capsize=5, label="Batch-GET x N parallel threads",
                zorder=3)

    ab_x = sorted(k for k in matched if k in nhints_sorted or k in (4, 8, 16, 32, 64))
    ab_y = [matched[k] for k in ab_x]
    ax.plot(ab_x, ab_y, "o--", color=ANYBLOB_COLOR, linewidth=2, markersize=7,
             label="AnyBlob individual GETs, matched connection count", zorder=3)

    ax.axhline(NET_CEILING_1CONN, color=CEILING_COLOR, linestyle="--", linewidth=1.0,
               label=f"Network ceiling, 1 conn ({NET_CEILING_1CONN:.0f} MB/s)")
    ax.axhline(NET_CEILING_8CONN, color=CEILING_COLOR, linestyle=":", linewidth=1.0,
               label=f"Network ceiling, 8 conns ({NET_CEILING_8CONN:.0f} MB/s)")
    ax.axvspan(8, 64, color=BATCH_COLOR, alpha=0.07, zorder=0)
    ax.annotate("plateau: 8→64 threads\nall within noise of each other",
                (22, 700), fontsize=8.5, color=BATCH_COLOR, ha="center",
                bbox=dict(boxstyle="round,pad=0.3", fc="white", ec=BATCH_COLOR, alpha=0.9))
    for x, y in zip(nhints_sorted, meds_sweep):
        ax.annotate(f"{y:.0f}", (x, y), textcoords="offset points", xytext=(0, -16),
                    ha="center", fontsize=7.5, color=BATCH_COLOR)
    ax.set_ylabel("Throughput (MB/s)")
    ax.set_title("A. Throughput vs thread/connection count (whiskers = min–max, n=10)", loc="left", fontsize=10)
    ax.legend(fontsize=8, loc="lower right")
    style_axis(ax, nhints_sorted)

    ax = axes[1]
    box_data = [sweep[n] for n in nhints_sorted]
    bp = ax.boxplot(box_data, positions=nhints_sorted, widths=[n * 0.25 for n in nhints_sorted],
                     patch_artist=True, showmeans=True,
                     boxprops=dict(facecolor=BATCH_COLOR, alpha=0.35),
                     medianprops=dict(color="black", linewidth=1.5),
                     meanprops=dict(marker="D", markerfacecolor="white", markeredgecolor="black"))
    ax.set_ylabel("Throughput (MB/s)")
    ax.set_xlabel("Number of parallel threads (= slab hints = connections)")
    ax.set_title("B. Full sample distribution per thread count — confirms the plateau isn't noise", loc="left", fontsize=10)
    style_axis(ax, nhints_sorted)

    fig2.tight_layout(rect=[0, 0, 1, 0.95])
    fig2.savefig(OUT / "02_parallel_scaling_story.png", dpi=150)
    plt.close(fig2)

    print("Recomputed summary (should match BATCH_GET_INVESTIGATION.md):")
    print(f"sequential median={statistics.median(seq_samples):.1f} "
          f"batch median={statistics.median(batch_samples):.1f} "
          f"gap={gap_pct:.1f}%")
    for n in nhints_sorted:
        print(f"nhints={n:3d} median={statistics.median(sweep[n]):.1f} "
              f"stdev={statistics.stdev(sweep[n]):.1f}")
    print(f"\nSaved plots to {OUT}/")


if __name__ == "__main__":
    main()
