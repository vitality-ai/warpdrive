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


def parse_worker_sweep(path, key_name):
    text = Path(path).read_text()
    out = {}
    for m in re.finditer(key_name + r"=\s*(\d+).*?samples=\[([^\]]+)\]", text):
        out[int(m.group(1))] = [float(x) for x in m.group(2).split(",")]
    return out


def main():
    single_conn_file = HERE / "single_conn_tightened_results.txt"
    batch_samples = parse_samples(single_conn_file, r"batch-GET, 256 objects \(2GB\), 1 connection ===\n")[0]
    seq_samples = parse_samples(single_conn_file, r"sequential 256x8MiB individual GETs, 1 connection ===\n")[0]

    sweep = parse_worker_sweep(HERE / "parallel_full_sweep_results.txt", "nhints")
    nhints_sorted = sorted(sweep.keys())

    # Fair, matched-connection-count, one-shot completion-time comparison
    # against REAL AnyBlob (not a naive Python ThreadPoolExecutor stand-in --
    # an earlier version of this script used plain `requests` + threads for
    # the "individual GETs" line, which understates what a properly
    # engineered client can do and overstated batching's win margin).
    # AnyBlob run with `-t nconn -c 1 -l 256`: nconn connections, one
    # outstanding request per connection, 256 total requests then stop --
    # a genuine one-shot fetch, not AnyBlob's *sustained* benchmark
    # throughput (which amortizes connection setup over thousands of
    # requests and isn't a fair stand-in for a one-time cold fetch).
    individual = parse_worker_sweep(HERE / "anyblob_oneshot_matched_results.txt", "nconn")
    individual_sorted = sorted(individual.keys())
    assert individual_sorted == nhints_sorted, "worker counts must match for a fair comparison"

    # Objects-per-worker table, tying the connection-count axis to a
    # concrete "how many delta layers of what size fit" framing.
    TOTAL_OBJS = 256
    OBJ_SIZE_MIB = 8
    per_worker_table = [(n, TOTAL_OBJS // n, (TOTAL_OBJS // n) * OBJ_SIZE_MIB) for n in nhints_sorted]

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
    # Figure 2: the crossover story -- two stacked panels, both holding
    # worker/connection count constant between batch and individual (the
    # fair comparison; see note above on why AnyBlob's sustained-benchmark
    # numbers aren't used here).
    #  A. throughput vs worker count, both approaches, one-shot completion
    #     time for the same 256 objects each time.
    #  B. % difference (batch vs individual) per worker count, so the
    #     crossover itself -- not just the two raw curves -- is the thing
    #     on screen.
    # ------------------------------------------------------------------
    fig2, axes = plt.subplots(2, 1, figsize=(9.5, 10.5))
    fig2.suptitle("Experiment 2 — batching vs. real AnyBlob, matched connection count\n"
                  "(both one-shot completion time for the same 256 objects / 2GB)",
                  fontsize=12.5, fontweight="bold")

    ax = axes[0]
    meds_batch = [statistics.median(sweep[n]) for n in nhints_sorted]
    mins_batch = [min(sweep[n]) for n in nhints_sorted]
    maxs_batch = [max(sweep[n]) for n in nhints_sorted]
    yerr_batch = [[m - lo for m, lo in zip(meds_batch, mins_batch)],
                  [hi - m for m, hi in zip(meds_batch, maxs_batch)]]
    ax.errorbar(nhints_sorted, meds_batch, yerr=yerr_batch, fmt="s-", color=BATCH_COLOR,
                linewidth=2, markersize=7, capsize=5, label="Batch-GET x N workers (1 request/worker)",
                zorder=3)

    meds_ind = [statistics.median(individual[n]) for n in individual_sorted]
    mins_ind = [min(individual[n]) for n in individual_sorted]
    maxs_ind = [max(individual[n]) for n in individual_sorted]
    yerr_ind = [[m - lo for m, lo in zip(meds_ind, mins_ind)],
                [hi - m for m, hi in zip(meds_ind, maxs_ind)]]
    ax.errorbar(individual_sorted, meds_ind, yerr=yerr_ind, fmt="o--", color=ANYBLOB_COLOR,
                linewidth=2, markersize=7, capsize=5,
                label="AnyBlob individual GETs, matched connections (one-shot, -t N -c 1 -l 256)", zorder=3)

    ax.axhline(NET_CEILING_1CONN, color=CEILING_COLOR, linestyle="--", linewidth=1.0,
               label=f"Network ceiling, 1 conn ({NET_CEILING_1CONN:.0f} MB/s)")
    ax.axhline(NET_CEILING_8CONN, color=CEILING_COLOR, linestyle=":", linewidth=1.0,
               label=f"Network ceiling, 8 conns ({NET_CEILING_8CONN:.0f} MB/s)")
    ax.axvspan(2, 5.6, color=BATCH_COLOR, alpha=0.08, zorder=0)
    ax.axvspan(5.6, 64, color=ANYBLOB_COLOR, alpha=0.06, zorder=0)
    ax.annotate("batching wins:\nfew connections,\nround trips dominate",
                (2.8, 1250), fontsize=8.5, color=BATCH_COLOR, ha="center",
                bbox=dict(boxstyle="round,pad=0.3", fc="white", ec=BATCH_COLOR, alpha=0.9))
    ax.annotate("AnyBlob wins:\nenough connections that\nio_uring concurrency dominates",
                (24, 500), fontsize=8.5, color=ANYBLOB_COLOR, ha="center",
                bbox=dict(boxstyle="round,pad=0.3", fc="white", ec=ANYBLOB_COLOR, alpha=0.9))
    for x, y in zip(nhints_sorted, meds_batch):
        ax.annotate(f"{y:.0f}", (x, y), textcoords="offset points", xytext=(0, -16),
                    ha="center", fontsize=7.5, color=BATCH_COLOR)
    for x, y in zip(individual_sorted, meds_ind):
        ax.annotate(f"{y:.0f}", (x, y), textcoords="offset points", xytext=(0, 8),
                    ha="center", fontsize=7.5, color=ANYBLOB_COLOR)
    ax.set_ylabel("Throughput (MB/s)")
    ax.set_title("A. Throughput vs connection count (whiskers = min–max, n=10)", loc="left", fontsize=10)
    ax.legend(fontsize=8, loc="center right")
    style_axis(ax, nhints_sorted)

    ax = axes[1]
    pct_diff = [100.0 * (b - i) / i for b, i in zip(meds_batch, meds_ind)]
    bar_colors = [BATCH_COLOR if p >= 0 else ANYBLOB_COLOR for p in pct_diff]
    xlabels = [f"{n}\n({objs} obj,\n{mib} MiB)" for n, objs, mib in per_worker_table]
    bars = ax.bar(xlabels, pct_diff, color=bar_colors,
                  edgecolor="black", linewidth=0.6, zorder=3)
    ax.axhline(0, color="black", linewidth=1.0, zorder=2)
    for bar, p in zip(bars, pct_diff):
        ax.annotate(f"{p:+.0f}%", (bar.get_x() + bar.get_width() / 2, p),
                    textcoords="offset points", xytext=(0, 6 if p >= 0 else -14),
                    ha="center", fontsize=9, fontweight="bold")
    lo_win = min(p for p in pct_diff if p >= 0)
    hi_win = max(p for p in pct_diff if p >= 0)
    lo_lose = max(p for p in pct_diff if p < 0)
    hi_lose = min(p for p in pct_diff if p < 0)
    ax.set_ylabel("Batch-GET advantage over real AnyBlob (%)")
    ax.set_xlabel("Connections (= workers)  —  objects per worker's single batch request  —  MiB per batch (8 MiB objects)")
    ax.set_title(f"B. The crossover — batching's advantage flips from +{lo_win:.0f}-{hi_win:.0f}% "
                 f"to {hi_lose:.0f}-{lo_lose:.0f}% between 4 and 8 connections", loc="left", fontsize=10)
    ax.grid(True, axis="y", which="major", linestyle="-", linewidth=0.8, color="#888888", alpha=0.85, zorder=0)
    ax.grid(True, axis="y", which="minor", linestyle=":", linewidth=0.6, color="#aaaaaa", alpha=0.6, zorder=0)
    ax.yaxis.set_minor_locator(mticker.AutoMinorLocator())
    ax.set_axisbelow(True)

    fig2.tight_layout(rect=[0, 0, 1, 0.95])
    fig2.savefig(OUT / "02_parallel_scaling_story.png", dpi=150)
    plt.close(fig2)

    print("Recomputed summary (should match BATCH_GET_INVESTIGATION.md):")
    print(f"sequential median={statistics.median(seq_samples):.1f} "
          f"batch median={statistics.median(batch_samples):.1f} "
          f"gap={gap_pct:.1f}%")
    for n in nhints_sorted:
        print(f"n={n:3d}  batch={statistics.median(sweep[n]):.1f}  "
              f"individual={statistics.median(individual[n]):.1f}  "
              f"diff={100.0*(statistics.median(sweep[n])-statistics.median(individual[n]))/statistics.median(individual[n]):+.1f}%")
    print(f"\nSaved plots to {OUT}/")


if __name__ == "__main__":
    main()
