#!/usr/bin/env python3
"""plot_engines.py <kvb dir> <kvb2 dir> <sizing dir> <out dir>: engine benchmark (Bitcask vs RocksDB: memory and PUT rate as
keys grow), the RocksDB read-option sweep (GET rate, p99, memory per config), and the Bitcask cluster sizing run (RSS per
node over the load). Separate charts, no shared axes."""
import csv, glob, os, sys
import matplotlib; matplotlib.use("Agg"); import matplotlib.pyplot as plt
kvb, kvb2, siz, out = sys.argv[1:5]
COL = {"bitcask": "#eb6834", "rocksdb": "#2a78d6"}; NODE = ["#2a78d6", "#eb6834", "#1baf7a"]
def style(ax, title, xl):
    ax.set_title(title, fontsize=10, loc="left"); ax.set_xlabel(xl, fontsize=8); ax.set_ylim(bottom=0)
    ax.grid(color="#e4e3df", lw=0.8); [ax.spines[s].set_visible(False) for s in ("top", "right")]
def rows(path, tag):
    return [l.strip().split(",") for l in open(path) if l.startswith(tag + ",") and ",engine," not in l]

# 1. Engines: RSS and PUT/s against keys loaded.
fig, axes = plt.subplots(1, 2, figsize=(12, 4.2), dpi=120)
for e in ("bitcask", "rocksdb"):
    ld = rows(f"{kvb}/{e}.out", "load")
    ks = [int(r[2]) / 1e6 for r in ld]
    axes[0].plot(ks, [float(r[8]) for r in ld], color=COL[e], lw=1.8, marker="o", ms=4, label=e)
    axes[1].plot(ks, [float(r[4]) for r in ld], color=COL[e], lw=1.8, marker="o", ms=4, label=e)
style(axes[0], "Memory (RSS) while loading, MB", "million rows (816 B each)")
style(axes[1], "Durable PUTs per second, 64 writers", "million rows")
axes[0].legend(fontsize=8, frameon=False)
fig.suptitle("Engine benchmark on wln-1 (n2-standard-2, 8 GB, NVMe): WarpMetaStore Bitcask vs RocksDB (256 MB cache)", fontsize=10, x=0.01, ha="left")
fig.tight_layout(); fig.savefig(f"{out}/engines.png"); plt.close(fig)

# 2. Sweep: GET/s, GET p99, RSS during GETs per config.
cfgs = sorted(os.path.basename(p)[:-4] for p in glob.glob(f"{kvb2}/*.out"))
g = {c: rows(f"{kvb2}/{c}.out", "get")[0] for c in cfgs}
labels = [c.split("_", 1)[1] for c in cfgs]
fig, axes = plt.subplots(1, 3, figsize=(15, 4.2), dpi=120)
for ax, col, title, scale in ((axes[0], 2, "GETs per second (thousands)", 1e-3), (axes[1], 4, "GET p99, ms", 1e-3), (axes[2], 7, "RSS during GETs, MB", 1)):
    vals = [float(g[c][col]) * scale for c in cfgs]
    ax.bar(range(len(cfgs)), vals, color=["#2a78d6" if c.startswith("B") else "#9fb9d9" for c in cfgs], width=0.7)
    ax.set_xticks(range(len(cfgs)), labels, fontsize=7, rotation=20); style(ax, title, "")
    for i, v in enumerate(vals): ax.text(i, v, f"{v:.0f}" if v >= 10 else f"{v:.1f}", ha="center", va="bottom", fontsize=7, color="#333")
fig.suptitle("RocksDB read options, 1 M rows, 64 threads (F: 16 threads); baseline A = 256 MB block cache, LZ4", fontsize=10, x=0.01, ha="left")
fig.tight_layout(); fig.savefig(f"{out}/rocksdb-read-options.png"); plt.close(fig)

# 3. Bitcask cluster sizing: RSS per node over the load, idle RSS after restart per slice.
lr = [r for r in csv.DictReader(open(f"{siz}/load_rss.csv")) if r["rss_kb"]]
t0 = min(int(r["unix_s"]) for r in lr)
fig, ax = plt.subplots(1, 1, figsize=(8, 4.2), dpi=120)
for n in range(3):
    pts = [((int(r["unix_s"]) - t0) / 60, int(r["rss_kb"]) / 1048576) for r in lr if int(r["node"]) == n]
    ax.plot([p[0] for p in pts], [p[1] for p in pts], color=NODE[n], lw=1.5, label=f"node {n}" + (" (15 GB)" if n == 0 else " (8 GB)"))
ax.axhline(6, color="#888", lw=1, ls="--"); ax.text(0.5, 6.05, "6 GB guard", fontsize=7, color="#666")
style(ax, "Bitcask cluster: memory (RSS) per node while loading new 4 KB objects, GB", "minute (slice 1 PUT then GET phases, restart, slice 2)")
ax.legend(fontsize=8, frameon=False, loc="lower right")
fig.tight_layout(); fig.savefig(f"{out}/sizing-bitcask.png"); plt.close(fig)
print(out)
