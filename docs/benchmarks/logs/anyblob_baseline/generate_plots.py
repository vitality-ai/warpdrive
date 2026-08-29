#!/usr/bin/env python3
"""
Generates the Experiment 1 (AnyBlob client-side baseline) plot set directly
from the raw AnyBlobBenchmark CSV summaries -- not from numbers transcribed
into the markdown report, so the plots and the report can't drift apart.

Usage:
    .venv/bin/python3 docs/benchmarks/logs/anyblob_baseline/generate_plots.py
"""
import csv
from pathlib import Path

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
import matplotlib.ticker as mticker

HERE = Path(__file__).resolve().parent
OUT = HERE / "plots"
OUT.mkdir(exist_ok=True)

# Independently measured ground truth (raw TCP sockets, no HTTP/S3 involved
# at all) -- see "Network ceiling" section of ANYBLOB_BASELINE.md.
NET_CEILING_1CONN = 1362.0   # MB/s, single raw TCP connection
NET_CEILING_8CONN = 1985.0   # MB/s, 8 parallel raw TCP connections

MINIO_COLOR = "#d95f02"
WARPDRIVE_COLOR = "#1b9e77"
CEILING_COLOR = "#555555"


def read_summary(path):
    rows = []
    with open(path) as f:
        for row in csv.DictReader(f):
            rows.append(row)
    return rows


def get_series(rows, step="Download"):
    """Returns (concurrency[], throughput_mbps[], cpu_pct[]) sorted by concurrency."""
    pts = []
    for r in rows:
        if r["Step"] != step:
            continue
        c = int(r["Concurrency"])
        time_ms = float(r["Time"])
        datasize = int(r["Datasize"])
        active = float(r["CPUActiveAllProcesses"])
        idle = float(r["CPUIdleAllProcesses"])
        throughput = (datasize / 1e6) / (time_ms / 1000.0)
        cpu_pct = 100.0 * active / (active + idle) if (active + idle) > 0 else 0.0
        pts.append((c, throughput, cpu_pct))
    pts.sort(key=lambda p: p[0])
    cs, tp, cpu = zip(*pts)
    return list(cs), list(tp), list(cpu)


def get_put_series(rows):
    """PUT-specific: Datasize column mislabels upload volume (uses the GET
    corpus's 8MiB object size instead of the real 16MiB upload size) -- a
    known AnyBlob client-reporting quirk documented in ANYBLOB_BASELINE.md.
    Recompute throughput from the real payload (Requests * 16 MiB)."""
    pts = []
    for r in rows:
        if r["Step"] != "Upload":
            continue
        c = int(r["Concurrency"])
        time_ms = float(r["Time"])
        requests = int(r["Requests"])
        real_bytes = requests * 16 * 1024 * 1024
        active = float(r["CPUActiveAllProcesses"])
        idle = float(r["CPUIdleAllProcesses"])
        throughput = (real_bytes / 1e6) / (time_ms / 1000.0)
        cpu_pct = 100.0 * active / (active + idle) if (active + idle) > 0 else 0.0
        pts.append((c, throughput, cpu_pct))
    pts.sort(key=lambda p: p[0])
    cs, tp, cpu = zip(*pts)
    return list(cs), list(tp), list(cpu)


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


def annotate_points(ax, xs, ys, color, fmt="{:.0f}", dy=1.0, va="bottom"):
    for x, y in zip(xs, ys):
        ax.annotate(fmt.format(y), (x, y), textcoords="offset points",
                     xytext=(0, 6 if va == "bottom" else -10), ha="center",
                     fontsize=7.5, color=color)


def first_crossing(xs, pct, threshold=94.5):
    # 94.5, not a strict 95.0: MinIO's own c=8 point lands at 94.8%, a
    # difference from 95% that's smaller than the run-to-run noise visible
    # elsewhere in this data (see CPU%/throughput stdevs in the report) --
    # treating that as "not yet saturated" would be spurious precision.
    for x, p in zip(xs, pct):
        if p >= threshold:
            return x
    return None


def main():
    minio_get = read_summary(HERE / "minio_get.csv.summary")
    wd_get = read_summary(HERE / "warpdrive_get.csv.summary")
    minio_put = read_summary(HERE / "minio_put.csv.summary")
    wd_put = read_summary(HERE / "warpdrive_put.csv.summary")

    mc, mtp, mcpu = get_series(minio_get)
    wc, wtp, wcpu = get_series(wd_get)
    assert mc == wc, "concurrency axes must match between targets"
    xvals = mc

    mpc, mptp, mpcpu = get_put_series(minio_put)
    wpc, wptp, wpcpu = get_put_series(wd_put)
    assert mpc == wpc
    pxvals = mpc

    ceiling_ref = (mtp[-3] + mtp[-2] + mtp[-1] + wtp[-3] + wtp[-2] + wtp[-1]) / 6.0
    m_pct = [100.0 * v / ceiling_ref for v in mtp]
    w_pct = [100.0 * v / ceiling_ref for v in wtp]

    # ------------------------------------------------------------------
    # Figure 1: GET story -- three stacked panels sharing the x-axis,
    # building the narrative: (A) raw throughput -> (B) why: % of the
    # shared ceiling reached, with saturation crossing points marked ->
    # (C) cost: client CPU utilization required to get there.
    # ------------------------------------------------------------------
    fig1, axes1 = plt.subplots(3, 1, figsize=(9, 12), sharex=True)
    fig1.suptitle("Experiment 1 — GET: MinIO vs WarpDrive under AnyBlob\n"
                  "(t=4 threads fixed, concurrency c swept 1→64; 8 MiB objects)",
                  fontsize=13, fontweight="bold")

    ax = axes1[0]
    ax.plot(xvals, mtp, "o-", color=MINIO_COLOR, label="MinIO", linewidth=2, markersize=6)
    ax.plot(xvals, wtp, "s-", color=WARPDRIVE_COLOR, label="WarpDrive", linewidth=2, markersize=6)
    ax.axhline(NET_CEILING_1CONN, color=CEILING_COLOR, linestyle="--", linewidth=1.2,
               label=f"Network ceiling, 1 raw TCP conn ({NET_CEILING_1CONN:.0f} MB/s)")
    ax.axhline(NET_CEILING_8CONN, color=CEILING_COLOR, linestyle=":", linewidth=1.2,
               label=f"Network ceiling, 8 raw TCP conns ({NET_CEILING_8CONN:.0f} MB/s)")
    annotate_points(ax, xvals, mtp, MINIO_COLOR)
    annotate_points(ax, xvals, wtp, WARPDRIVE_COLOR, va="top")
    ax.set_ylabel("Throughput (MB/s)")
    ax.set_title("A. Raw throughput", loc="left", fontsize=10)
    ax.legend(fontsize=8, loc="lower right")
    style_axis(ax, xvals)

    ax = axes1[1]
    ax.plot(xvals, m_pct, "o-", color=MINIO_COLOR, label="MinIO", linewidth=2, markersize=6)
    ax.plot(xvals, w_pct, "s-", color=WARPDRIVE_COLOR, label="WarpDrive", linewidth=2, markersize=6)
    ax.axhline(94.5, color=CEILING_COLOR, linestyle="--", linewidth=1.0, label="~95% saturation threshold (94.5%)")
    m_cross = first_crossing(xvals, m_pct)
    w_cross = first_crossing(xvals, w_pct)
    if w_cross:
        ax.axvline(w_cross, color=WARPDRIVE_COLOR, linestyle=":", linewidth=1.2, alpha=0.7)
        ax.annotate(f"WarpDrive saturates\nat c={w_cross}", (w_cross, 60),
                    color=WARPDRIVE_COLOR, fontsize=8, ha="center",
                    bbox=dict(boxstyle="round,pad=0.3", fc="white", ec=WARPDRIVE_COLOR, alpha=0.9))
    if m_cross:
        ax.axvline(m_cross, color=MINIO_COLOR, linestyle=":", linewidth=1.2, alpha=0.7)
        ax.annotate(f"MinIO saturates\nat c={m_cross}", (m_cross, 30),
                    color=MINIO_COLOR, fontsize=8, ha="center",
                    bbox=dict(boxstyle="round,pad=0.3", fc="white", ec=MINIO_COLOR, alpha=0.9))
    annotate_points(ax, xvals, m_pct, MINIO_COLOR, fmt="{:.0f}%")
    annotate_points(ax, xvals, w_pct, WARPDRIVE_COLOR, fmt="{:.0f}%", va="top")
    ax.set_ylabel("% of shared ceiling\n(avg of top-3 concurrency points)")
    ax.set_title("B. Saturation point — how much concurrency does each system actually need?", loc="left", fontsize=10)
    ax.legend(fontsize=8, loc="lower right")
    ax.set_ylim(0, 115)
    style_axis(ax, xvals)

    ax = axes1[2]
    ax.plot(xvals, mcpu, "o-", color=MINIO_COLOR, label="MinIO", linewidth=2, markersize=6)
    ax.plot(xvals, wcpu, "s-", color=WARPDRIVE_COLOR, label="WarpDrive", linewidth=2, markersize=6)
    annotate_points(ax, xvals, mcpu, MINIO_COLOR, fmt="{:.1f}%")
    annotate_points(ax, xvals, wcpu, WARPDRIVE_COLOR, fmt="{:.1f}%", va="top")
    ax.set_ylabel("Client-machine CPU utilization (%)")
    ax.set_xlabel("Concurrency c (outstanding requests per thread; t=4 threads fixed)")
    ax.set_title("C. Cost — whole-client-machine CPU required to reach that throughput", loc="left", fontsize=10)
    ax.legend(fontsize=8, loc="upper left")
    style_axis(ax, xvals)

    fig1.tight_layout(rect=[0, 0, 1, 0.96])
    fig1.savefig(OUT / "01_get_story.png", dpi=150)
    plt.close(fig1)

    # ------------------------------------------------------------------
    # Figure 2: PUT story -- two stacked panels, same x-axis.
    # ------------------------------------------------------------------
    fig2, axes2 = plt.subplots(2, 1, figsize=(9, 8.5), sharex=True)
    fig2.suptitle("Experiment 1 — PUT: MinIO vs WarpDrive under AnyBlob\n"
                  "(t=4 threads fixed, concurrency c swept 1→64; fixed 16 MiB uploads)",
                  fontsize=13, fontweight="bold")

    ax = axes2[0]
    ax.plot(pxvals, mptp, "o-", color=MINIO_COLOR, label="MinIO", linewidth=2, markersize=6)
    ax.plot(pxvals, wptp, "s-", color=WARPDRIVE_COLOR, label="WarpDrive", linewidth=2, markersize=6)
    peak_idx = wptp.index(max(wptp))
    ax.annotate(f"WarpDrive peak\n{wptp[peak_idx]:.0f} MB/s @ c={pxvals[peak_idx]}",
                (pxvals[peak_idx], wptp[peak_idx]), textcoords="offset points",
                xytext=(45, -55), fontsize=8, color=WARPDRIVE_COLOR,
                bbox=dict(boxstyle="round,pad=0.3", fc="white", ec=WARPDRIVE_COLOR, alpha=0.9),
                arrowprops=dict(arrowstyle="->", color=WARPDRIVE_COLOR))
    ax.annotate(f"collapses to {wptp[-1]:.0f} MB/s\nby c={pxvals[-1]}",
                (pxvals[-1], wptp[-1]), textcoords="offset points",
                xytext=(-95, 55), fontsize=8, color=WARPDRIVE_COLOR,
                bbox=dict(boxstyle="round,pad=0.3", fc="white", ec=WARPDRIVE_COLOR, alpha=0.9),
                arrowprops=dict(arrowstyle="->", color=WARPDRIVE_COLOR))
    annotate_points(ax, pxvals, mptp, MINIO_COLOR)
    ax.set_ylabel("Throughput (MB/s)")
    ax.set_title("A. Raw throughput — MinIO flat (disk/checksum-bound); WarpDrive peaks then degrades", loc="left", fontsize=10)
    ax.legend(fontsize=8, loc="center right")
    style_axis(ax, pxvals)

    ax = axes2[1]
    ax.plot(pxvals, mpcpu, "o-", color=MINIO_COLOR, label="MinIO", linewidth=2, markersize=6)
    ax.plot(pxvals, wpcpu, "s-", color=WARPDRIVE_COLOR, label="WarpDrive", linewidth=2, markersize=6)
    annotate_points(ax, pxvals, mpcpu, MINIO_COLOR, fmt="{:.1f}%")
    annotate_points(ax, pxvals, wpcpu, WARPDRIVE_COLOR, fmt="{:.1f}%", va="top")
    ax.annotate("client CPU drops while\nthroughput also drops:\nclient is waiting, not\nworking -> bottleneck is\nserver/network, not client",
                (pxvals[-2], wpcpu[-2]), textcoords="offset points", xytext=(-140, 30),
                fontsize=7.5, color=WARPDRIVE_COLOR,
                bbox=dict(boxstyle="round,pad=0.3", fc="white", ec=WARPDRIVE_COLOR, alpha=0.9),
                arrowprops=dict(arrowstyle="->", color=WARPDRIVE_COLOR))
    ax.set_ylabel("Client-machine CPU utilization (%)")
    ax.set_xlabel("Concurrency c (outstanding requests per thread; t=4 threads fixed)")
    ax.set_title("B. Cost/diagnostic — client CPU stays low even as WarpDrive throughput collapses", loc="left", fontsize=10)
    ax.legend(fontsize=8, loc="upper right")
    style_axis(ax, pxvals)

    fig2.tight_layout(rect=[0, 0, 1, 0.96])
    fig2.savefig(OUT / "02_put_story.png", dpi=150)
    plt.close(fig2)

    # ------------------------------------------------------------------
    # Figure 3: combined summary panel -- GET and PUT throughput side by
    # side for a single at-a-glance comparison, plus the ceiling context.
    # ------------------------------------------------------------------
    fig3, axes3 = plt.subplots(1, 2, figsize=(13, 5.5))
    fig3.suptitle("Experiment 1 — summary: GET vs PUT throughput scaling", fontsize=13, fontweight="bold")

    ax = axes3[0]
    ax.plot(xvals, mtp, "o-", color=MINIO_COLOR, label="MinIO GET", linewidth=2, markersize=6)
    ax.plot(xvals, wtp, "s-", color=WARPDRIVE_COLOR, label="WarpDrive GET", linewidth=2, markersize=6)
    ax.axhline(NET_CEILING_1CONN, color=CEILING_COLOR, linestyle="--", linewidth=1.0, alpha=0.8)
    ax.axhline(NET_CEILING_8CONN, color=CEILING_COLOR, linestyle=":", linewidth=1.0, alpha=0.8)
    ax.set_ylabel("Throughput (MB/s)")
    ax.set_xlabel("Concurrency c")
    ax.set_title("GET: both converge to the network ceiling", fontsize=10)
    ax.legend(fontsize=8)
    style_axis(ax, xvals)

    ax = axes3[1]
    ax.plot(pxvals, mptp, "o-", color=MINIO_COLOR, label="MinIO PUT", linewidth=2, markersize=6)
    ax.plot(pxvals, wptp, "s-", color=WARPDRIVE_COLOR, label="WarpDrive PUT", linewidth=2, markersize=6)
    ax.set_ylabel("Throughput (MB/s)")
    ax.set_xlabel("Concurrency c")
    ax.set_title("PUT: MinIO flat, WarpDrive peaks then degrades", fontsize=10)
    ax.legend(fontsize=8)
    style_axis(ax, pxvals)

    fig3.tight_layout(rect=[0, 0, 1, 0.93])
    fig3.savefig(OUT / "03_get_vs_put_summary.png", dpi=150)
    plt.close(fig3)

    # Print the recomputed numbers so they can be cross-checked against
    # the markdown report by eye.
    print("Recomputed from raw CSVs (should match ANYBLOB_BASELINE.md):")
    print(f"{'c':>4} {'MinIO GET':>10} {'WD GET':>10} {'MinIO GET%':>11} {'WD GET%':>9} "
          f"{'MinIO CPU%':>11} {'WD CPU%':>9}")
    for i, c in enumerate(xvals):
        print(f"{c:>4} {mtp[i]:>10.1f} {wtp[i]:>10.1f} {m_pct[i]:>10.1f}% {w_pct[i]:>8.1f}% "
              f"{mcpu[i]:>10.1f}% {wcpu[i]:>8.1f}%")
    print()
    print(f"{'c':>4} {'MinIO PUT':>10} {'WD PUT':>10} {'MinIO CPU%':>11} {'WD CPU%':>9}")
    for i, c in enumerate(pxvals):
        print(f"{c:>4} {mptp[i]:>10.1f} {wptp[i]:>10.1f} {mpcpu[i]:>10.1f}% {wpcpu[i]:>8.1f}%")

    print(f"\nSaved plots to {OUT}/")


if __name__ == "__main__":
    main()
