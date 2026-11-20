#!/usr/bin/env python3
"""Microseconds of node0 CPU per request, by theme. Usage: cpu_per_request.py <prof dir> <out dir>
CPU per request = perf stat task-clock / requests completed in the same 10 s window."""
import csv, re, sys
import matplotlib; matplotlib.use("Agg")
import matplotlib.pyplot as plt
P, OUT = sys.argv[1], sys.argv[2]
ops = {(r["phase"], r["kind"]): int(r["ops"]) for r in csv.DictReader(open(f"{P}/window_ops.csv"))}
THEMES = [  # theme, categories from cpu_breakdown.py
    ("Talking to other nodes (gRPC + kernel TCP)", ["gRPC to peers (tonic, h2, hyper, prost)", "kernel: network (TCP, virtio)"]),
    ("Thread hand-offs (scheduler, futex, runtime, locks)", ["kernel: scheduler, futex, wakeups", "async runtime and locks (tokio, parking_lot)"]),
    ("malloc, free, copies", ["malloc, free, copies"]),
    ("S3 front end (HTTP parsing + routing)", ["HTTP/1 parsing and responses (actix-http)", "HTTP routing (actix router, regex)"]),
    ("Hashing (SigV4, SHA-256, MD5, checksum)", ["hashing: SHA-256, MD5, checksums"]),
    ("Disk (ext4, NVMe, fdatasync)", ["kernel: storage (ext4, NVMe, sync)"]),
    ("WarpDrive logic, JSON, erasure coding", ["WarpDrive code", "JSON (serde_json)", "erasure coding"]),
    ("Other (tracing, clock, page faults, misc)", ["other user space", "kernel: other", "tracing spans (no output at WARN)", "clock reads", "kernel: memory (page faults, mmap)"]),
]
rows, per = [], {}
for ph in ("put", "get"):
    txt = open(f"{P}/{ph}-perfstat.txt").read()
    ms = float(re.search(r"([\d.]+)\s+msec task-clock", txt).group(1))
    cs = int(re.search(r"(\d+)\s+context-switches", txt).group(1))
    n = ops[(ph, "stat")]
    per[ph] = {"cpu_us": ms * 1000 / n, "cs": cs / n}
    share = {r["category"]: float(r["share_pct"]) for r in csv.DictReader(open(f"{P}/{ph}-node0-categories.csv"))}
    for t, cats in THEMES:
        pct = sum(share.get(c, 0) for c in cats)
        rows.append({"phase": ph, "theme": t, "share_pct": round(pct, 1), "us_per_request": round(pct / 100 * per[ph]["cpu_us"])})
with open(f"{OUT}/cpu_per_request.csv", "w", newline="") as f:
    w = csv.DictWriter(f, fieldnames=list(rows[0])); w.writeheader(); w.writerows(rows)
INK, MUTED, GRID = "#1f2328", "#5b636e", "#e3e6ea"
plt.rcParams.update({"font.size": 9.5, "axes.edgecolor": GRID, "axes.labelcolor": MUTED, "xtick.color": MUTED, "ytick.color": MUTED,
                     "axes.grid": True, "grid.color": GRID, "axes.spines.top": False, "axes.spines.right": False})
fig, ax = plt.subplots(1, 2, figsize=(14, 4.8), sharey=True)
for a, ph, col in ((ax[0], "put", "#2a78d6"), (ax[1], "get", "#1baf7a")):
    rs = [r for r in rows if r["phase"] == ph]
    bars = a.barh([r["theme"] for r in rs], [r["us_per_request"] for r in rs], color=col, height=0.62)
    for b, r in zip(bars, rs):
        a.text(b.get_width(), b.get_y() + b.get_height() / 2, f" {r['us_per_request']} µs ({r['share_pct']:.0f}%)", va="center", fontsize=8.5, color=INK)
    a.grid(axis="y", visible=False); a.set_xlim(0, 330)
    a.set_title(f"{ph.upper()}: {per[ph]['cpu_us']:.0f} µs of node0 CPU per request, {per[ph]['cs']:.0f} context switches", loc="left", color=INK, fontsize=10.5)
    a.set_xlabel("µs of CPU per request")
ax[0].invert_yaxis()  # shared y axis: invert once, largest theme on top
fig.suptitle("Where node0's CPU goes per 4 KB request. #226 b62e763, RS(2,1), node0 pinned to 2 cores, 128 clients. perf (dwarf stacks) + perf stat, GCP n2-standard-4",
             x=0.01, ha="left", color=MUTED, fontsize=9)
fig.tight_layout(rect=(0, 0, 1, 0.94)); fig.savefig(f"{OUT}/cpu_per_request.png", dpi=150)
for ph in ("put", "get"):
    print(ph, {k: round(v, 1) for k, v in per[ph].items()})
    for r in rows:
        if r["phase"] == ph: print(f"   {r['us_per_request']:4d} µs  {r['share_pct']:5.1f}%  {r['theme']}")
