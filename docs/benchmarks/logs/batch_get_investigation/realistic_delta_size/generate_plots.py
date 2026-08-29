#!/usr/bin/env python3
"""
Generates the realistic-delta-layer-size plot: does batching still help
once object size matches Neon's real DEFAULT_CHECKPOINT_DISTANCE (256 MiB,
libs/pageserver_api/src/config.rs), instead of the 8 MiB objects used in
the rest of Experiment 2?

Parses the raw sample arrays directly out of the committed results .txt
files.

Usage:
    .venv/bin/python3 docs/benchmarks/logs/batch_get_investigation/realistic_delta_size/generate_plots.py
"""
import re
import statistics
from pathlib import Path

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
import matplotlib.ticker as mticker

HERE = Path(__file__).resolve().parent
OUT = HERE / "plots"
OUT.mkdir(exist_ok=True)

BATCH_COLOR = "#1b9e77"
ANYBLOB_COLOR = "#d95f02"

N_OBJECTS = 8
OBJ_SIZE_MIB = 256


def parse_worker_sweep(path, key_name):
    text = Path(path).read_text()
    out = {}
    for m in re.finditer(key_name + r"=\s*(\d+).*?samples=\[([^\]]+)\]", text):
        out[int(m.group(1))] = [float(x) for x in m.group(2).split(",")]
    return out


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


def main():
    # n=1 case comes from single_conn_results.txt (different label format:
    # "batch-GET (8 layers, 1 request)" / "sequential individual GETs").
    single_text = (HERE / "single_conn_results.txt").read_text()
    batch_1 = eval(re.search(r"batch-GET.*?\n(\{.*?\})", single_text, re.S).group(1))["samples"]
    seq_1 = eval(re.search(r"sequential.*?\n(\{.*?\})", single_text, re.S).group(1))["samples"]

    batch_sweep = parse_worker_sweep(HERE / "batch_parallel_sweep_results.txt", "batch nworkers")
    batch_sweep[1] = batch_1
    nvals = sorted(batch_sweep.keys())

    anyblob_sweep = parse_worker_sweep(HERE / "anyblob_oneshot_results.txt", "nconn")
    anyblob_sweep[1] = seq_1  # n=1 AnyBlob one-shot == sequential single-connection
    assert sorted(anyblob_sweep.keys()) == nvals

    fig, axes = plt.subplots(2, 1, figsize=(9.5, 10))
    fig.suptitle("Realistic delta-layer size: does batching still help?\n"
                  f"({N_OBJECTS} x {OBJ_SIZE_MIB} MiB = {N_OBJECTS*OBJ_SIZE_MIB/1024:.1f} GiB, matching Neon's "
                  "DEFAULT_CHECKPOINT_DISTANCE — vs. 8 MiB objects elsewhere in Exp. 2)",
                  fontsize=10.5, fontweight="bold")

    ax = axes[0]
    meds_b = [statistics.median(batch_sweep[n]) for n in nvals]
    mins_b = [min(batch_sweep[n]) for n in nvals]
    maxs_b = [max(batch_sweep[n]) for n in nvals]
    yerr_b = [[m - lo for m, lo in zip(meds_b, mins_b)], [hi - m for m, hi in zip(meds_b, maxs_b)]]
    ax.errorbar(nvals, meds_b, yerr=yerr_b, fmt="s-", color=BATCH_COLOR, linewidth=2,
                markersize=7, capsize=5, label="Batch-GET x N workers", zorder=3)

    meds_a = [statistics.median(anyblob_sweep[n]) for n in nvals]
    mins_a = [min(anyblob_sweep[n]) for n in nvals]
    maxs_a = [max(anyblob_sweep[n]) for n in nvals]
    yerr_a = [[m - lo for m, lo in zip(meds_a, mins_a)], [hi - m for m, hi in zip(meds_a, maxs_a)]]
    ax.errorbar(nvals, meds_a, yerr=yerr_a, fmt="o--", color=ANYBLOB_COLOR, linewidth=2,
                markersize=7, capsize=5, label="Individual GETs, matched connections\n(AnyBlob one-shot at n>1, sequential at n=1)", zorder=3)

    for x, y in zip(nvals, meds_b):
        ax.annotate(f"{y:.0f}", (x, y), textcoords="offset points", xytext=(0, -16),
                    ha="center", fontsize=8, color=BATCH_COLOR)
    for x, y in zip(nvals, meds_a):
        ax.annotate(f"{y:.0f}", (x, y), textcoords="offset points", xytext=(0, 8),
                    ha="center", fontsize=8, color=ANYBLOB_COLOR)
    ax.set_ylabel("Throughput (MB/s)")
    ax.set_title("A. Individual GETs win at every connection count tested", loc="left", fontsize=10)
    ax.legend(fontsize=8, loc="upper left")
    style_axis(ax, nvals)

    ax = axes[1]
    pct_diff = [100.0 * (b - a) / a for b, a in zip(meds_b, meds_a)]
    objs_per_worker = [N_OBJECTS // n for n in nvals]
    xlabels = [f"{n}\n({o} obj,\n{o*OBJ_SIZE_MIB} MiB)" for n, o in zip(nvals, objs_per_worker)]
    bars = ax.bar(xlabels, pct_diff, color=ANYBLOB_COLOR, edgecolor="black", linewidth=0.6, zorder=3)
    ax.axhline(0, color="black", linewidth=1.0, zorder=2)
    for bar, p in zip(bars, pct_diff):
        ax.annotate(f"{p:.0f}%", (bar.get_x() + bar.get_width() / 2, p),
                    textcoords="offset points", xytext=(0, -14), ha="center", fontsize=9, fontweight="bold")
    ax.set_ylabel("Batch-GET advantage over individual GET (%)")
    ax.set_xlabel("Connections  —  objects per worker's batch  —  MiB per batch (256 MiB objects)")
    ax.set_title("B. Batching's disadvantage: consistently -49% to -63%, never positive", loc="left", fontsize=10)
    ax.grid(True, axis="y", which="major", linestyle="-", linewidth=0.8, color="#888888", alpha=0.85, zorder=0)
    ax.grid(True, axis="y", which="minor", linestyle=":", linewidth=0.6, color="#aaaaaa", alpha=0.6, zorder=0)
    ax.yaxis.set_minor_locator(mticker.AutoMinorLocator())
    ax.set_axisbelow(True)

    fig.tight_layout(rect=[0, 0, 1, 0.93])
    fig.savefig(OUT / "01_realistic_size_comparison.png", dpi=150)
    plt.close(fig)

    print("Recomputed summary:")
    for n in nvals:
        b, a = statistics.median(batch_sweep[n]), statistics.median(anyblob_sweep[n])
        print(f"n={n}: batch={b:.1f} individual={a:.1f} diff={100.0*(b-a)/a:+.1f}%")
    print(f"\nSaved to {OUT}/")


if __name__ == "__main__":
    main()
