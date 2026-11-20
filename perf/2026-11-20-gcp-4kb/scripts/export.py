#!/usr/bin/env python3
"""Turn one long run's raw output into clean CSVs and plots.

Usage: export.py <raw res dir> <out dir> <title>

Raw layout (from longrun.sh): <raw>/<config>/{bench.csv, persec.csv, syncs.txt, mem0..4.csv}.
Writes:
  summary.csv             one row per configuration
  per_second.csv          config, phase, second, ops, errors, p50_ms, p99_ms
  node_memory.csv         config, node, second, rss_mb, data_mb[, cpu_busy_pct, nvme_busy_pct]
  throughput.png, latency_p99.png, memory.png, summary.png
"""
import csv, os, statistics as st, sys
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt

RAW, OUT, TITLE = sys.argv[1], sys.argv[2], sys.argv[3]
os.makedirs(OUT, exist_ok=True)
ORDER = ["217", "217i8", "par", "par_i4", "par_i8"]
LABEL = {"217": "#217", "217i8": "#217 + inline 8 KB", "par": "#226", "par_i4": "#226 + inline 4 KB", "par_i8": "#226 + inline 8 KB"}
COLOR = {"217": "#8a8f98", "217i8": "#3f4650", "par": "#2a78d6", "par_i4": "#1baf7a", "par_i8": "#eb6834"}
DASH = {"217": (0, (4, 2)), "217i8": (0, (4, 2)), "par": "-", "par_i4": "-", "par_i8": "-"}
INK, MUTED, GRID = "#1f2328", "#5b636e", "#e3e6ea"
plt.rcParams.update({"font.size": 9.5, "axes.edgecolor": GRID, "axes.labelcolor": MUTED, "xtick.color": MUTED, "ytick.color": MUTED,
                     "axes.grid": True, "grid.color": GRID, "axes.spines.top": False, "axes.spines.right": False,
                     "figure.facecolor": "white", "legend.frameon": False})
configs = [c for c in ORDER if os.path.exists(f"{RAW}/{c}/bench.csv")]

# ---- CSVs
summary = []
for c in configs:
    rows = {l.split(",")[0]: l.strip().split(",") for l in open(f"{RAW}/{c}/bench.csv") if l[:3] in ("put", "get")}
    syncs = int((open(f"{RAW}/{c}/syncs.txt").read().strip() or "0"))
    p, g = rows["put"], rows["get"]
    mem_peak = 0
    for i in range(5):
        path = f"{RAW}/{c}/mem{i}.csv"
        if os.path.exists(path):
            for l in open(path):
                f = l.strip().split(",")
                if len(f) >= 3 and f[0].isdigit() and f[1]:
                    mem_peak = max(mem_peak, int(f[1]) / 1024)
    summary.append({
        "config": c, "label": LABEL[c], "clients": p[2], "object_bytes": p[1],
        "put_ops": p[3], "put_errors": p[4], "put_per_s": p[5], "put_p50_ms": int(p[7]) / 1000, "put_p99_ms": int(p[9]) / 1000,
        "get_ops": g[3], "get_errors": g[4], "get_per_s": g[5], "get_p50_ms": int(g[7]) / 1000, "get_p99_ms": int(g[9]) / 1000,
        "syncs_per_put": round(syncs / int(p[3]), 2) if int(p[3]) else 0, "peak_node_rss_mb": round(mem_peak),
    })
with open(f"{OUT}/summary.csv", "w", newline="") as f:
    w = csv.DictWriter(f, fieldnames=list(summary[0].keys()))
    w.writeheader(); w.writerows(summary)

per_sec = {}
with open(f"{OUT}/per_second.csv", "w", newline="") as f:
    w = csv.writer(f); w.writerow(["config", "phase", "second", "ops", "errors", "p50_ms", "p99_ms"])
    for c in configs:
        rows = list(csv.DictReader(open(f"{RAW}/{c}/persec.csv")))
        for ph in ("put", "get"):
            rs = sorted((int(r["sec"]), int(r["ops"]), int(r["errors"]), int(r["p50_us"]), int(r["p99_us"])) for r in rows if r["phase"] == ph)
            if not rs:
                continue
            base = rs[0][0]
            # Drop the last second: it's partial (the phase ends mid-second).
            rs = rs[:-1]
            per_sec[(c, ph)] = [(s - base, o, p99 / 1000) for s, o, e, p50, p99 in rs]
            for s, o, e, p50, p99 in rs:
                w.writerow([c, ph, s - base, o, e, p50 / 1000, p99 / 1000])

mem = {}
with open(f"{OUT}/node_memory.csv", "w", newline="") as f:
    w = csv.writer(f); w.writerow(["config", "node", "second", "rss_mb", "data_mb", "cpu_busy_pct", "nvme_busy_pct"])
    for c in configs:
        for i in range(5):
            path = f"{RAW}/{c}/mem{i}.csv"
            if not os.path.exists(path):
                continue
            rows = [l.strip().split(",") for l in open(path)]
            rows = [r for r in rows if len(r) >= 3 and r[0].isdigit()]
            if not rows:
                continue
            t0 = int(rows[0][0])
            series = []
            for r in rows:
                cpu, nvme = (r[3], r[4]) if len(r) >= 5 else ("", "")
                rss = int(r[1] or 0) / 1024
                w.writerow([c, i, int(r[0]) - t0, round(rss, 1), round(int(r[2] or 0) / 1024, 1), cpu, nvme])
                series.append((int(r[0]) - t0, rss))
            mem[(c, i)] = series


def smooth(xs, n=15):
    return [st.mean(xs[max(0, i - n + 1): i + 1]) for i in range(len(xs))]


def style(ax, title, ylabel, xlabel):
    ax.set_title(title, loc="left", color=INK, fontsize=10.5)
    ax.set_ylabel(ylabel); ax.set_xlabel(xlabel); ax.set_ylim(bottom=0)


# ---- throughput over time
fig, ax = plt.subplots(1, 2, figsize=(13, 4.6))
for c in configs:
    for j, ph in enumerate(("put", "get")):
        s = per_sec.get((c, ph))
        if s:
            ax[j].plot([x[0] for x in s], smooth([x[1] for x in s]), color=COLOR[c], ls=DASH[c], lw=2, label=LABEL[c])
style(ax[0], "PUT throughput", "PUT/s (15 s moving average)", "seconds into the PUT phase")
style(ax[1], "GET throughput, reading back everything written", "GET/s (15 s moving average)", "seconds into the GET phase")
h, l = ax[0].get_legend_handles_labels()
fig.legend(h, l, loc="lower center", ncol=len(l), fontsize=9)
fig.suptitle(TITLE, x=0.01, ha="left", color=MUTED, fontsize=9)
fig.tight_layout(rect=(0, 0.07, 1, 0.93)); fig.savefig(f"{OUT}/throughput.png", dpi=150); plt.close(fig)

# ---- p99 latency over time
fig, ax = plt.subplots(1, 2, figsize=(13, 4.6))
for c in configs:
    for j, ph in enumerate(("put", "get")):
        s = per_sec.get((c, ph))
        if s:
            ax[j].plot([x[0] for x in s], smooth([x[2] for x in s]), color=COLOR[c], ls=DASH[c], lw=2, label=LABEL[c])
style(ax[0], "PUT p99 latency", "ms (15 s moving average)", "seconds into the PUT phase")
style(ax[1], "GET p99 latency", "ms (15 s moving average)", "seconds into the GET phase")
h, l = ax[0].get_legend_handles_labels()
fig.legend(h, l, loc="lower center", ncol=len(l), fontsize=9)
fig.suptitle(TITLE, x=0.01, ha="left", color=MUTED, fontsize=9)
fig.tight_layout(rect=(0, 0.07, 1, 0.93)); fig.savefig(f"{OUT}/latency_p99.png", dpi=150); plt.close(fig)

# ---- memory on the fullest node
fig, ax = plt.subplots(figsize=(10, 4.8))
for c in configs:
    nodes = [mem[(c, i)] for i in range(5) if (c, i) in mem]
    if nodes:
        peak = max(nodes, key=lambda s: max(x[1] for x in s))
        ax.plot([x[0] / 60 for x in peak], [x[1] / 1024 for x in peak], color=COLOR[c], ls=DASH[c], lw=2, label=LABEL[c])
style(ax, "Server memory (RSS) on the fullest node", "GB", "minutes since the cluster started (PUT phase, then GET)")
ax.axhline(8, color=MUTED, lw=1, ls=":"); ax.set_ylim(0, 8.8)
ax.text(ax.get_xlim()[1], 8.12, "VM RAM 8 GB ", color=MUTED, fontsize=8.5, ha="right")
ax.legend(loc="center right", fontsize=8.5)
fig.suptitle(TITLE, x=0.01, ha="left", color=MUTED, fontsize=8.5, wrap=True)
fig.tight_layout(rect=(0, 0, 1, 0.93)); fig.savefig(f"{OUT}/memory.png", dpi=150); plt.close(fig)

# ---- summary bars
fig, ax = plt.subplots(1, 3, figsize=(13, 4.2))
labels = [LABEL[s["config"]] for s in summary]
cols = [COLOR[s["config"]] for s in summary]
for a, key, title, unit in ((ax[0], "put_per_s", "PUT/s, whole phase", "PUT/s"), (ax[1], "get_per_s", "GET/s, whole phase", "GET/s"),
                            (ax[2], "syncs_per_put", "fsyncs per PUT, all 5 nodes", "syncs")):
    vals = [float(s[key]) for s in summary]
    bars = a.barh(labels, vals, color=cols, height=0.6)
    a.invert_yaxis(); a.set_title(title, loc="left", color=INK, fontsize=10.5); a.set_xlabel(unit)
    a.grid(axis="y", visible=False)
    for b, v in zip(bars, vals):
        a.text(b.get_width(), b.get_y() + b.get_height() / 2, f" {v:,.0f}" if v >= 20 else f" {v:.1f}", va="center", fontsize=8.5, color=INK)
    a.set_xlim(0, max(vals) * 1.18 if vals else 1)
for a in ax[1:]:
    a.set_yticklabels([])
fig.suptitle(TITLE, x=0.01, ha="left", color=MUTED, fontsize=9)
fig.tight_layout(rect=(0, 0, 1, 0.92)); fig.savefig(f"{OUT}/summary.png", dpi=150); plt.close(fig)

print(f"wrote {OUT}: summary.csv per_second.csv node_memory.csv throughput.png latency_p99.png memory.png summary.png")
