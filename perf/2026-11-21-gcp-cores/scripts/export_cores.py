#!/usr/bin/env python3
"""Core-count test: summary.csv and cores.png. Usage: export_cores.py <raw cores dir> <out dir>"""
import csv, glob, os, re, sys
import matplotlib; matplotlib.use("Agg")
import matplotlib.pyplot as plt
R, OUT = sys.argv[1], sys.argv[2]
def busy(path):
    rows = [l.strip().split(",") for l in open(path) if l.count(",") == 4]
    rows = [(int(r[3]), int(r[4])) for r in rows if r[0].isdigit()][1:-1]
    return (sum(r[0] for r in rows) / len(rows), sum(r[1] for r in rows) / len(rows)) if rows else (0, 0)
rows = []
for d in sorted(glob.glob(f"{R}/*_c*")):
    topo, cores, clients = re.match(r"(\w+?)_(\d)c_c(\d+)$", os.path.basename(d)).groups()
    b = {l.split(",")[0]: l.strip().split(",") for l in open(f"{d}/bench.csv") if l[:3] in ("put", "get")}
    cpu, nvme = busy(f"{d}/mem0.csv"); ccpu, _ = busy(f"{d}/client.csv")
    p, g = b["put"], b["get"]
    rows.append({"topology": {"single": "single node", "rs21": "RS(2,1), 3 nodes"}[topo], "node0_cores": int(cores), "clients": int(clients),
                 "put_per_s": float(p[5]), "put_p50_ms": int(p[7]) / 1000, "put_p99_ms": int(p[9]) / 1000, "put_errors": int(p[4]),
                 "get_per_s": float(g[5]), "get_p50_ms": int(g[7]) / 1000, "get_p99_ms": int(g[9]) / 1000, "get_errors": int(g[4]),
                 "node0_cores_busy": round(cpu / 100 * 4, 2), "node0_nvme_busy_pct": round(nvme), "client_cpu_pct": round(ccpu)})
with open(f"{OUT}/summary.csv", "w", newline="") as f:
    w = csv.DictWriter(f, fieldnames=list(rows[0])); w.writeheader(); w.writerows(rows)
INK, MUTED, GRID = "#1f2328", "#5b636e", "#e3e6ea"
plt.rcParams.update({"font.size": 9.5, "axes.edgecolor": GRID, "axes.labelcolor": MUTED, "xtick.color": MUTED, "ytick.color": MUTED,
                     "axes.grid": True, "grid.color": GRID, "axes.spines.top": False, "axes.spines.right": False, "legend.frameon": False})
fig, ax = plt.subplots(1, 3, figsize=(14, 4.4))
groups = [(t, c) for t in ("single node", "RS(2,1), 3 nodes") for c in (128, 512)]
xl = [f"{t}\n{c} clients" for t, c in groups]; x = range(len(groups)); wd = 0.36
for a, key, title in ((ax[0], "put_per_s", "PUT/s, 4 KB"), (ax[1], "get_per_s", "GET/s, 4 KB"), (ax[2], "node0_cores_busy", "node0 cores busy (of 2 or 4 allowed)")):
    for j, (cores, col) in enumerate(((2, "#8a8f98"), (4, "#2a78d6"))):
        v = [next(r[key] for r in rows if r["topology"] == t and r["clients"] == c and r["node0_cores"] == cores) for t, c in groups]
        bars = a.bar([i + (j - 0.5) * wd for i in x], v, wd, color=col, label=f"node0 on {cores} cores")
        for bb, vv in zip(bars, v):
            a.text(bb.get_x() + bb.get_width() / 2, bb.get_height(), f"{vv:,.0f}" if vv > 20 else f"{vv:.1f}", ha="center", va="bottom", fontsize=8, color=INK)
    a.set_xticks(list(x)); a.set_xticklabels(xl, fontsize=8); a.set_title(title, loc="left", color=INK); a.grid(axis="x", visible=False)
ax[0].legend(loc="upper left", fontsize=8.5)
fig.suptitle("#226 (b62e763) on GCP: node0 n2-standard-4 pinned to 2 or 4 cores with taskset, local NVMe, 4 KB objects, 120 s PUT then 120 s GET, 4-vCPU client",
             x=0.01, ha="left", color=MUTED, fontsize=9)
fig.tight_layout(rect=(0, 0, 1, 0.93)); fig.savefig(f"{OUT}/cores.png", dpi=150)
print(open(f"{OUT}/summary.csv").read())
