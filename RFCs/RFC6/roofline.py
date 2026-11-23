#!/usr/bin/env python3
"""RFC 6 roofline: how close each compaction configuration gets to the
hardware limits, at each machine size.

Usage: roofline.py <out_dir> <label>=<results_dir> [<label>=<results_dir> ...]

Each results_dir has ceilings.jsonl (fio) and scale.jsonl (segbench runs).

Every customer byte written costs WA bytes of device writes, so
    customer write MB/s  <=  device write ceiling / WA
WA here is what the device actually wrote (iostat wMB/s / customer MB/s),
not the bytes compaction copied: on XFS, copy_file_range shares blocks, so a
copy can cost no device writes at all. The x axis is customer bytes per
device byte written (1 / device WA), so the disk ceiling is a line through
the origin with slope = the fio write bandwidth.

The CPU ceiling comes from the runs themselves: CPU % per MB/s of device
traffic (writes + compaction + reads), so the most device traffic the
machine's cores can carry is  cores * 100 / (CPU % per MB/s). Reads run at
a fixed rate, so their share comes off the top before dividing by WA.
"""
import glob, json, os, statistics, sys
from collections import defaultdict
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt

OUT = sys.argv[1]
os.makedirs(OUT, exist_ok=True)
MEAN_OBJ_MB = (0.6 * 64 * 1024 + 0.3 * (1 << 20) + 0.1 * (4 << 20)) / 1e6  # segbench object mix
READ_RATE = 200  # reads per second, segbench default for these runs

sizes = []
d_of = {}
for arg in sys.argv[2:]:
    label, d = arg.split("=", 1)
    d_of[label] = d
    ceil = {j["test"]: j for j in (json.loads(l) for l in open(os.path.join(d, "ceilings.jsonl")))}
    runs = [json.loads(l) for l in open(os.path.join(d, "scale.jsonl")) if l.strip()]
    cores = int(open(os.path.join(d, "ceilings.txt")).read().split("=")[1])
    sizes.append((label, cores, ceil, runs))

PAL = ["#2a78d6", "#eb6834", "#1baf7a", "#eda100"]
INK, MUTED, GRID = "#1f2328", "#5b636e", "#e3e6ea"
plt.rcParams.update({"font.size": 9, "axes.edgecolor": GRID, "axes.labelcolor": MUTED, "xtick.color": MUTED,
                     "ytick.color": MUTED, "axes.grid": True, "grid.color": GRID, "axes.spines.top": False,
                     "axes.spines.right": False, "figure.facecolor": "white"})
fig, axes = plt.subplots(1, len(sizes), figsize=(5.2 * len(sizes), 4.6), squeeze=False)
table = []
for k, (label, cores, ceil, runs) in enumerate(sizes):
    ax = axes[0, k]
    w_ceiling = ceil["write_sync"]["write_mbs"]
    read_mbs = READ_RATE * MEAN_OBJ_MB
    by = defaultdict(list)
    for r in runs:
        by[r["name"]].append(r["r"])
    def device_write_mbs(name):
        """Mean device write MB/s over each run's measured window (iostat), median over trials."""
        vals = []
        for f in glob.glob(os.path.join(d_of[label], "mon", f"{name}-t*.iostat")):
            hdr, w = None, []
            for l in open(f):
                t = l.split()
                if t and t[0].startswith("Device"):
                    hdr = t
                elif hdr and t and t[0].startswith("nvme"):
                    w.append(float(dict(zip(hdr, t))["wMB/s"]) * 1.048576)
            w = w[len(w) // 3:]
            if w:
                vals.append(statistics.mean(w))
        return statistics.median(vals) if vals else None

    pts = []
    for name, rs in by.items():
        fg0 = statistics.median(x["fg_mbs"] for x in rs)
        dw = device_write_mbs(name)
        wa = dw / fg0 if dw else statistics.median(x["wa"] for x in rs)
        fg = statistics.median(x["fg_mbs"] for x in rs)
        cpu = statistics.median(x["cpu_pct"] for x in rs)
        device = fg * wa + read_mbs
        pts.append((name, 1 / wa, fg, cpu, device))
    # CPU cost per MB/s of device traffic: median over compacting runs.
    c_per = statistics.median(p[3] / p[4] for p in pts if p[4] > 0)
    cpu_device_cap = cores * 100 / c_per
    xs = [i / 1000 for i in range(800, 1021)]
    cpu_line = max(cpu_device_cap - read_mbs, 0)
    ax.plot(xs, [w_ceiling * x for x in xs], color=INK, lw=1.8, label=f"disk ceiling: fio write+fsync {w_ceiling:.0f} MB/s x (1 / WA)")
    GROUPS = [("real", "real copies (read/write, or ext4)", PAL[3], "D"),
              ("shared", "XFS copy_file_range (blocks shared)", PAL[0], "o"),
              ("nocompact", "no compaction", PAL[1], "s")]
    def group(n):
        return "nocompact" if n == "nocompact" else ("real" if (n == "copy-rw" or n.startswith("ext4")) else "shared")
    for g, glabel, col, mk in GROUPS:
        gp = [p for p in pts if group(p[0]) == g]
        if gp:
            ax.plot([p[1] for p in gp], [p[2] for p in gp], ls="none", marker=mk, ms=10 if g == "nocompact" else 7,
                    color=col, mec=INK if g == "nocompact" else "white", mew=1, label=glabel, zorder=4 if g == "nocompact" else 3)
    for name, x, y, cpu, dev in pts:
        bound = min(w_ceiling * x, cpu_line * x)
        limiter = "disk" if w_ceiling * x <= cpu_line * x else "CPU"
        table.append((label, name, 1 / x, y, cpu, bound, y / bound if bound else 0, limiter))
    ax.text(1.015, w_ceiling * 0.08, f"CPU ceiling ~{cpu_line:,.0f} MB/s x (1 / WA),\noff chart ({cores} cores)", fontsize=7.5, color=MUTED, ha="right")
    ax.set_ylim(0, w_ceiling * 1.3)
    ax.set_xlabel("customer bytes per device byte written (1 / device WA)")
    ax.set_ylabel("customer write MB/s")
    ax.set_title(f"{label}", loc="left", color=INK)
    ax.set_xlim(0.8, 1.02)
    ax.legend(loc="upper left", fontsize=7.5, frameon=False)
fig.suptitle("Roofline: customer write throughput against the disk ceiling. Every run is disk-bound; CPU has >10x headroom.", x=0.01, ha="left", color=INK)
fig.tight_layout(rect=(0, 0, 1, 0.94))
fig.savefig(os.path.join(OUT, "roofline.png"), dpi=150)

with open(os.path.join(OUT, "roofline.md"), "w") as f:
    f.write("| Size | Run | device WA | customer MB/s | process CPU % | ceiling MB/s | share of ceiling | binding ceiling |\n|---|---|---|---|---|---|---|---|\n")
    for row in sorted(table):
        f.write("| {} | {} | {:.2f} | {:.0f} | {:.0f} | {:.0f} | {:.0%} | {} |\n".format(*row))
    for label, cores, ceil, _ in sizes:
        f.write(f"\n{label}: " + ", ".join(f"{t} {c['write_mbs'] or c['read_mbs']:.0f} MB/s" for t, c in ceil.items()))
print(open(os.path.join(OUT, "roofline.md")).read())
