#!/usr/bin/env python3
"""Saturation sweep plots. Usage: sweep_plot.py <sweep dir> <out png>

<sweep dir>/<variant>_c<clients>/ has bench.csv and mem0..4.csv
(unix_s,rss_kb,data_kb,cpu_busy_pct,nvme_busy_pct). <sweep dir>/client-<name>.csv
has the client's own CPU in the same format.
"""
import glob, os, re, statistics as st, sys
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt

D, OUT = sys.argv[1], sys.argv[2]
LABEL = {"217": "#217", "par": "this PR (32 HTTP workers)", "par_w256": "this PR, 256 HTTP workers"}
COLOR = {"217": "#8a8f98", "par": "#2a78d6", "par_w256": "#eb6834"}
INK, MUTED, GRID = "#1f2328", "#5b636e", "#e3e6ea"
plt.rcParams.update({"font.size": 9, "axes.edgecolor": GRID, "axes.labelcolor": MUTED, "xtick.color": MUTED, "ytick.color": MUTED,
                     "axes.grid": True, "grid.color": GRID, "axes.spines.top": False, "axes.spines.right": False, "figure.facecolor": "white"})


def bench(path):
    rows = {l.split(",")[0]: l.strip().split(",") for l in open(path) if l[:3] in ("put", "get")}
    return {ph: (float(r[5]), int(r[7]) / 1000, int(r[9]) / 1000, int(r[4])) for ph, r in rows.items()}


def util(path):
    """Median CPU and NVMe busy % over the samples in the middle of the run."""
    rows = [l.strip().split(",") for l in open(path) if l.count(",") == 4]
    rows = [(int(r[3]), int(r[4])) for r in rows if r[0].isdigit()]
    mid = rows[len(rows) // 5: -max(1, len(rows) // 5)] or rows
    return (st.median(r[0] for r in mid), st.median(r[1] for r in mid)) if mid else (0, 0)


points = {}
for d in glob.glob(f"{D}/*_c*"):
    m = re.match(r"(.+)_c(\d+)$", os.path.basename(d))
    if not m or not os.path.exists(f"{d}/bench.csv"):
        continue
    v, c = m.group(1), int(m.group(2))
    b = bench(f"{d}/bench.csv")
    nodes = [util(p) for p in sorted(glob.glob(f"{d}/mem*.csv"))]
    client = f"{D}/client-{os.path.basename(d)}.csv"
    points.setdefault(v, []).append({
        "c": c, "put": b.get("put", (0, 0, 0, 0)), "get": b.get("get", (0, 0, 0, 0)),
        "cpu0": nodes[0][0] if nodes else 0, "nvme0": nodes[0][1] if nodes else 0,
        "cpu_max": max((n[0] for n in nodes), default=0),
        "client_cpu": util(client)[0] if os.path.exists(client) else 0,
    })

variants = [v for v in ("217", "par", "par_w256") if v in points]
fig, ax = plt.subplots(2, 3, figsize=(15, 8.2))
for v in variants:
    ps = sorted(points[v], key=lambda p: p["c"])
    cs = [p["c"] for p in ps]
    kw = dict(color=COLOR[v], marker="o", ms=5, lw=1.8, label=LABEL[v])
    ax[0, 0].plot(cs, [p["put"][0] for p in ps], **kw)
    ax[0, 1].plot(cs, [p["get"][0] for p in ps], **kw)
    ax[1, 0].plot(cs, [p["put"][2] for p in ps], **kw)
    ax[1, 1].plot(cs, [p["get"][2] for p in ps], **kw)
    ax[0, 2].plot(cs, [p["cpu0"] for p in ps], **kw)
    ax[0, 2].plot(cs, [p["nvme0"] for p in ps], color=COLOR[v], marker="s", ms=4, lw=1.2, ls=":")
    ax[1, 2].plot(cs, [p["client_cpu"] for p in ps], **kw)
for a in ax.flat:
    a.set_xscale("log", base=2)
    a.set_xticks([32, 64, 128, 256, 512]); a.set_xticklabels(["32", "64", "128", "256", "512"])
    a.set_xlabel("concurrent clients")
    a.set_ylim(bottom=0)
ax[0, 0].set_title("PUT throughput, 4 KB", loc="left", color=INK); ax[0, 0].set_ylabel("PUT/s")
ax[0, 1].set_title("GET throughput, 4 KB", loc="left", color=INK); ax[0, 1].set_ylabel("GET/s")
ax[1, 0].set_title("PUT p99 latency", loc="left", color=INK); ax[1, 0].set_ylabel("ms")
ax[1, 1].set_title("GET p99 latency", loc="left", color=INK); ax[1, 1].set_ylabel("ms")
ax[0, 2].set_title("Coordinator node (node0): CPU busy (solid), NVMe busy (dotted)", loc="left", color=INK, fontsize=8.5)
ax[0, 2].set_ylabel("% busy, median over the run"); ax[0, 2].set_ylim(0, 105)
ax[1, 2].set_title("Load-generating client: CPU busy", loc="left", color=INK); ax[1, 2].set_ylabel("% busy"); ax[1, 2].set_ylim(0, 105)
ax[0, 0].legend(loc="upper left", fontsize=7.5, frameon=False)
fig.suptitle("Saturation sweep: 5-node RS(3,2) on GCP n2-standard-2 with local NVMe, one 2-vCPU client, WARN logging. "
             "A flat curve with an idle resource panel means something else is the limit.", x=0.01, ha="left", color=INK, fontsize=9)
fig.tight_layout(rect=(0, 0, 1, 0.95))
fig.savefig(OUT, dpi=150)

print("| variant | clients | PUT/s | PUT p99 ms | GET/s | GET p99 ms | errors | node0 CPU % | node0 NVMe % | busiest node CPU % | client CPU % |")
print("|---|---|---|---|---|---|---|---|---|---|---|")
for v in variants:
    for p in sorted(points[v], key=lambda p: p["c"]):
        print(f"| {LABEL[v]} | {p['c']} | {p['put'][0]:.0f} | {p['put'][2]:.0f} | {p['get'][0]:.0f} | {p['get'][2]:.0f} | "
              f"{p['put'][3] + p['get'][3]} | {p['cpu0']:.0f} | {p['nvme0']:.0f} | {p['cpu_max']:.0f} | {p['client_cpu']:.0f} |")
