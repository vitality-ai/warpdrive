#!/usr/bin/env python3
"""Turn RFC 5 benchmark results into plots and a summary table.

Input: a results directory from .github/ci-free bench.sh runs, laid out as
  <results>/summary.csv
  <results>/<run>/{bench.csv,client_sar.txt,node<i>/{sar.txt,iostat.txt,traces.txt,timing.txt}}
Output: PNG plots and results.md in <out>.

Usage: analyze.py <results_dir> <out_dir>
"""
import csv, os, re, statistics, sys
from collections import defaultdict

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
import matplotlib.ticker

RES, OUT = sys.argv[1], sys.argv[2]
os.makedirs(OUT, exist_ok=True)

# Reference categorical palette, fixed order (validated: all hard checks pass,
# contrast WARN on slots 3-4, so every series also gets a distinct marker and
# results.md carries the same numbers as a table).
VARIANTS = ["B0", "B1", "B2", "B3"]
LABEL = {
    "B0": "B0 main",
    "B1": "B1 prototype, switches off",
    "B2": "B2 + inline small objects",
    "B3": "B3 + inline + skip local sync",
}
COLOR = {"B0": "#2a78d6", "B1": "#eb6834", "B2": "#1baf7a", "B3": "#eda100"}
MARKER = {"B0": "o", "B1": "s", "B2": "^", "B3": "D"}
INK, INK2, GRID, SURFACE = "#0b0b0b", "#52514e", "#e4e3df", "#fcfcfb"

plt.rcParams.update({
    "figure.facecolor": SURFACE, "axes.facecolor": SURFACE, "savefig.facecolor": SURFACE,
    "axes.edgecolor": GRID, "axes.labelcolor": INK2, "xtick.color": INK2, "ytick.color": INK2,
    "axes.grid": True, "axes.axisbelow": True, "grid.color": GRID, "grid.linewidth": 0.8, "axes.spines.top": False,
    "axes.spines.right": False, "font.size": 10, "axes.titlesize": 11, "axes.titlecolor": INK,
    "legend.frameon": False, "lines.linewidth": 2, "lines.markersize": 7,
})


def size_label(b):
    return f"{b // 1024} KB" if b < 1 << 20 else f"{b // (1 << 20)} MB"


# ---------------------------------------------------------------- summary
rows = list(csv.DictReader(open(os.path.join(RES, "summary.csv"))))
for r in rows:
    for k in ("payload_bytes", "concurrency", "ops", "errors", "mean_us", "p50_us", "p90_us", "p99_us", "syncs_total", "trial"):
        r[k] = int(float(r[k]))
    r["throughput_ops_s"] = float(r["throughput_ops_s"])

TRIALS = tuple(int(t) for t in os.environ.get("ANALYZE_TRIALS", "1,2").split(","))
size_rows = [r for r in rows if r["concurrency"] == 16 and r["trial"] in TRIALS and not r["run"].endswith("_perf")]
SIZES = sorted({r["payload_bytes"] for r in size_rows})


def agg(phase, metric, variant, size, conc=16, data=size_rows):
    vals = [r[metric] for r in data if r["phase"] == phase and r["variant"] == variant
            and r["payload_bytes"] == size and r["concurrency"] == conc]
    return statistics.mean(vals) if vals else None


def spread(phase, metric, variant, size):
    vals = [r[metric] for r in size_rows if r["phase"] == phase and r["variant"] == variant and r["payload_bytes"] == size]
    return (min(vals), max(vals)) if vals else (None, None)


def line_panel(ax, phase, metric, title, ylabel, scale_us=True):
    for v in VARIANTS:
        ys = [agg(phase, metric, v, s) for s in SIZES]
        pts = [(s, y / 1000 if scale_us else y) for s, y in zip(SIZES, ys) if y is not None]
        if not pts:
            continue
        ax.plot([p[0] for p in pts], [p[1] for p in pts], color=COLOR[v], marker=MARKER[v], label=LABEL[v])
    ax.set_xscale("log", base=2)
    ax.set_xticks(SIZES, [size_label(s) for s in SIZES])
    # Log scale: object sizes span 1000x, and the small sizes are where the
    # variants differ. A linear axis would flatten them under the 1 MB points.
    ax.set_yscale("log")
    ax.yaxis.set_major_formatter(matplotlib.ticker.FuncFormatter(lambda y, _: f"{y:g}"))
    ax.yaxis.set_minor_formatter(matplotlib.ticker.NullFormatter())
    ax.set_title(title + " (log scale)", loc="left")
    ax.set_ylabel(ylabel)
    ax.axvline(65536, color=INK2, linewidth=1, linestyle=":")
    ax.text(65536, ax.get_ylim()[1], " inline threshold", color=INK2, fontsize=8, va="top")


# Latency (2x2) and throughput (1x2)
fig, axes = plt.subplots(2, 2, figsize=(12, 8))
line_panel(axes[0][0], "put", "p50_us", "PUT p50 latency", "ms")
line_panel(axes[0][1], "put", "p99_us", "PUT p99 latency", "ms")
line_panel(axes[1][0], "get", "p50_us", "GET p50 latency", "ms")
line_panel(axes[1][1], "get", "p99_us", "GET p99 latency", "ms")
axes[0][0].legend(loc="upper left")
fig.suptitle("Latency by object size, 5-node RS(3,2), concurrency 16, mean of 2 trials", x=0.01, ha="left", color=INK)
fig.tight_layout(); fig.savefig(os.path.join(OUT, "latency.png"), dpi=150); plt.close(fig)

fig, axes = plt.subplots(1, 2, figsize=(12, 4.2))
line_panel(axes[0], "put", "throughput_ops_s", "PUT throughput", "ops/s", scale_us=False)
line_panel(axes[1], "get", "throughput_ops_s", "GET throughput", "ops/s", scale_us=False)
axes[0].legend(loc="lower left")
fig.suptitle("Throughput by object size, 5-node RS(3,2), concurrency 16, mean of 2 trials", x=0.01, ha="left", color=INK)
fig.tight_layout(); fig.savefig(os.path.join(OUT, "throughput.png"), dpi=150); plt.close(fig)

# Syncs per PUT (only the PUT phase writes, so syncs_total / put ops)
fig, ax = plt.subplots(figsize=(12, 4))
w = 0.2
for i, v in enumerate(["B1", "B2", "B3"]):
    ys = []
    for s in SIZES:
        put = [r for r in size_rows if r["phase"] == "put" and r["variant"] == v and r["payload_bytes"] == s]
        ys.append(statistics.mean(r["syncs_total"] / r["ops"] for r in put) if put else 0)
    xs = [j + (i - 1) * w for j in range(len(SIZES))]
    ax.bar(xs, ys, w - 0.02, color=COLOR[v], label=LABEL[v], edgecolor=SURFACE, linewidth=2)
ax.set_xticks(range(len(SIZES)), [size_label(s) for s in SIZES])
ax.set_ylabel("syncs per PUT, all 5 nodes")
ax.set_title("Disk syncs per PUT, all nodes (main has no counter; B1 is main's code path with the counter added)", loc="left")
ax.legend(loc="upper right")
fig.tight_layout(); fig.savefig(os.path.join(OUT, "syncs_per_put.png"), dpi=150); plt.close(fig)

# ---------------------------------------------------------------- traces
def trace_means(runs):
    """Stage means over every PUT trace line in the given runs (all trials)."""
    acc = defaultdict(list)
    for run in runs:
      f = os.path.join(RES, run, "node0", "traces.txt")
      if not os.path.exists(f):
          continue
      for line in open(f):
          if "trace put" not in line:
              continue
          for k in ("shards_us", "local_cleanup_us", "replicate_us", "total_us"):
              m = re.search(rf"{k}=(\d+)", line)
              if m:
                  acc[k].append(int(m.group(1)))
    return {k: statistics.mean(v) for k, v in acc.items()} if acc else None


fig, axes = plt.subplots(1, 2, figsize=(12, 4.2))
STAGES = [("shards_us", "shard round"), ("local_cleanup_us", "local cleanup sync"), ("replicate_us", "record round")]
STAGE_COLOR = ["#2a78d6", "#eb6834", "#1baf7a"]
breakdown = {}
for ax, s in zip(axes, [4096, 1048576]):
    for i, v in enumerate(["B1", "B2", "B3"]):
        t = trace_means([f"{v}_s{s}_c16_t{tr}" for tr in TRIALS])
        if not t:
            continue
        breakdown[(v, s)] = t
        bottom = 0
        for (k, name), c in zip(STAGES, STAGE_COLOR):
            val = t.get(k, 0) / 1000
            ax.bar(i, val, 0.6, bottom=bottom, color=c, edgecolor=SURFACE, linewidth=2, label=name if i == 0 else None)
            bottom += val
        ax.text(i, bottom, f"{bottom:.1f} ms", ha="center", va="bottom", color=INK, fontsize=9)
    ax.set_xticks(range(3), [LABEL[v].split(" ", 1)[1] for v in ["B1", "B2", "B3"]], fontsize=8)
    ax.set_ylabel("ms, coordinator view")
    ax.set_title(f"Where a PUT's time goes, {size_label(s)}", loc="left")
handles, labels = axes[0].get_legend_handles_labels()
fig.legend(handles, labels, loc="lower center", ncol=3)
fig.subplots_adjust(bottom=0.2)
fig.suptitle("Server-side PUT stages from per-request traces on the coordinator (node0), all PUTs of both trials", x=0.01, ha="left", color=INK)
fig.tight_layout(rect=(0, 0.08, 1, 1)); fig.savefig(os.path.join(OUT, "put_breakdown.png"), dpi=150); plt.close(fig)

# ---------------------------------------------------------------- resources
def sod(hms):
    h, m, x = (int(v) for v in hms.split(":"))
    return h * 3600 + m * 60 + x


def windows(run):
    """UTC seconds-of-day [start, end] of the PUT and GET phases, from the client trace."""
    w = {}
    f = os.path.join(RES, run, "client_trace.csv")
    for line in open(f):
        phase, start, lat = line.split(",")[:3]
        st, en = int(start) / 1e6, (int(start) + int(lat)) / 1e6
        lo, hi = w.get(phase, (st, en))
        w[phase] = (min(lo, st), max(hi, en))
    return {k: (v[0] % 86400, v[1] % 86400) for k, v in w.items()}


def parse_sar(path):
    """(seconds-of-day, sample) for CPU rows and for the NIC's network rows."""
    cpu, net = [], []
    for line in open(path, errors="ignore"):
        t = line.split()
        if len(t) < 6 or not re.fullmatch(r"\d\d:\d\d:\d\d", t[0]):
            continue
        try:
            if t[1] == "all":
                cpu.append((sod(t[0]), {"busy": 100 - float(t[7]), "iowait": float(t[5])}))
            elif t[1] in ("ens4", "ens5", "eth0"):
                net.append((sod(t[0]), {"rx_kBs": float(t[4]), "tx_kBs": float(t[5])}))
        except (ValueError, IndexError):
            pass
    return cpu, net


def parse_iostat(path):
    """(seconds-of-day, row) per interval. The first block is since-boot, so it's skipped."""
    cols, cur, blocks, out = None, None, 0, []
    for line in open(path, errors="ignore"):
        m = re.match(r"^\d\d/\d\d/\d\d (\d\d:\d\d:\d\d)", line)
        if m:
            cur, blocks = sod(m.group(1)), blocks + 1
            continue
        t = line.split()
        if t and t[0].startswith("Device"):
            cols = t
        elif cols and t and t[0].startswith("nvme") and blocks > 1:
            out.append((cur, dict(zip(cols, t))))
    return out


def in_window(samples, win):
    lo, hi = win
    return [x for ts, x in samples if lo <= ts <= hi + 1]


def mean(xs):
    return statistics.mean(xs) if xs else 0.0


def fnum(row, k):
    try:
        return float(row.get(k, 0))
    except ValueError:
        return 0.0


def resources(run):
    res = {"put": defaultdict(list), "get": defaultdict(list)}
    if not os.path.exists(os.path.join(RES, run, "client_trace.csv")):
        return {p: {} for p in res}
    win = windows(run)
    for i in range(5):
        d = os.path.join(RES, run, f"node{i}")
        if not os.path.isdir(d):
            continue
        cpu, net = parse_sar(os.path.join(d, "sar.txt"))
        io = parse_iostat(os.path.join(d, "iostat.txt"))
        for phase in ("put", "get"):
            if phase not in win:
                continue
            c = in_window(cpu, win[phase]); n = in_window(net, win[phase]); x = in_window(io, win[phase])
            res[phase]["cpu"].append(mean([s["busy"] for s in c]))
            res[phase]["iowait"].append(mean([s["iowait"] for s in c]))
            res[phase]["net_MBs"].append(mean([(s["rx_kBs"] + s["tx_kBs"]) / 1024 for s in n]))
            res[phase]["disk_wMBs"].append(mean([fnum(s, "wMB/s") for s in x]))
            res[phase]["disk_w_s"].append(mean([fnum(s, "w/s") for s in x]))
            res[phase]["disk_flush_s"].append(mean([fnum(s, "f/s") for s in x]))
            res[phase]["disk_util"].append(mean([fnum(s, "%util") for s in x]))
    return {p: {k: mean(v) for k, v in m.items()} for p, m in res.items()}


RES_METRICS = [("cpu", "CPU busy %, mean of nodes"), ("net_MBs", "network rx+tx MB/s, mean of nodes"),
               ("disk_w_s", "disk writes/s, mean of nodes"), ("disk_flush_s", "disk flushes/s, mean of nodes")]
res_table = {}
for phase in ("put", "get"):
    fig, axes = plt.subplots(1, 4, figsize=(15, 3.8))
    for ax, (k, title) in zip(axes, RES_METRICS):
        for i, v in enumerate(VARIANTS):
            r = resources(f"{v}_s4096_c16_t{TRIALS[-1]}")
            res_table[(phase, v)] = r[phase]
            val = r[phase].get(k, 0)
            ax.bar(i, val, 0.6, color=COLOR[v], edgecolor=SURFACE, linewidth=2)
            ax.text(i, val, f"{val:.0f}" if val >= 10 else f"{val:.1f}", ha="center", va="bottom", color=INK, fontsize=8)
        ax.set_xticks(range(4), VARIANTS)
        ax.set_title(title, loc="left", fontsize=9)
    fig.suptitle(f"Per-node resources during the {phase.upper()} phase, 4 KB, concurrency 16 "
                 f"(B0 main, B1 switches off, B2 inline, B3 inline + skip local sync), trial {TRIALS[-1]}", x=0.01, ha="left", color=INK)
    fig.tight_layout(); fig.savefig(os.path.join(OUT, f"resources_{phase}.png"), dpi=150); plt.close(fig)

# ---------------------------------------------------------------- concurrency
conc_rows = [r for r in rows if r["payload_bytes"] == 4096 and r["trial"] == 1 and r["variant"] in ("B0", "B3") and not r["run"].endswith("_perf")]
CONCS = sorted({r["concurrency"] for r in conc_rows})
fig, axes = plt.subplots(1, 4, figsize=(16, 3.8))
for ax, (phase, metric, title, scale) in zip(axes, [
        ("put", "throughput_ops_s", "PUT throughput, ops/s", 1), ("put", "p99_us", "PUT p99, ms", 1000),
        ("get", "throughput_ops_s", "GET throughput, ops/s", 1), ("get", "p99_us", "GET p99, ms", 1000)]):
    for v in ("B0", "B3"):
        pts = [(c, agg(phase, metric, v, 4096, c, conc_rows)) for c in CONCS]
        pts = [(c, y / scale) for c, y in pts if y is not None]
        ax.plot([p[0] for p in pts], [p[1] for p in pts], color=COLOR[v], marker=MARKER[v], label=LABEL[v])
    ax.set_xscale("log", base=2); ax.set_xticks(CONCS, [str(c) for c in CONCS])
    ax.set_xlabel("client concurrency"); ax.set_title(title, loc="left")
axes[0].legend(loc="upper left")
fig.suptitle("Scaling with concurrency, 4 KB objects, 1 trial", x=0.01, ha="left", color=INK)
fig.tight_layout(); fig.savefig(os.path.join(OUT, "concurrency.png"), dpi=150); plt.close(fig)

# ---------------------------------------------------------------- table
def fmt(x, ms=False):
    if x is None: return "n/a"
    return f"{x / 1000:.2f}" if ms else f"{x:,.0f}"

with open(os.path.join(OUT, "results.md"), "w") as f:
    f.write("| Size | Variant | PUT ops/s | PUT p50 ms | PUT p99 ms | GET ops/s | GET p50 ms | GET p99 ms | syncs/PUT (cluster) |\n")
    f.write("|---|---|---|---|---|---|---|---|---|\n")
    for s in SIZES:
        for v in VARIANTS:
            put = [r for r in size_rows if r["phase"] == "put" and r["variant"] == v and r["payload_bytes"] == s]
            spp = statistics.mean(r["syncs_total"] / r["ops"] for r in put) if put else None
            spp_txt = "n/a (main has no counter)" if v == "B0" else (f"{spp:.2f}" if spp is not None else "n/a")
            f.write(f"| {size_label(s)} | {v} | {fmt(agg('put','throughput_ops_s',v,s))} | {fmt(agg('put','p50_us',v,s),1)} | "
                    f"{fmt(agg('put','p99_us',v,s),1)} | {fmt(agg('get','throughput_ops_s',v,s))} | {fmt(agg('get','p50_us',v,s),1)} | "
                    f"{fmt(agg('get','p99_us',v,s),1)} | {spp_txt} |\n" if put else "")
    f.write("\n**Trial spread** (min-max of the 2 trials, PUT ops/s at 4 KB): ")
    f.write(", ".join(f"{v} {fmt(spread('put','throughput_ops_s',v,4096)[0])}-{fmt(spread('put','throughput_ops_s',v,4096)[1])}" for v in VARIANTS) + "\n\n")
    f.write("**Resources at 4 KB, concurrency 16, mean per node** (CPU %, network MB/s, disk writes/s, disk flushes/s):\n\n")
    f.write("| Phase | Variant | CPU % | net MB/s | disk writes/s | disk flushes/s |\n|---|---|---|---|---|---|\n")
    for (phase, v), r in sorted(res_table.items()):
        f.write(f"| {phase} | {v} | {r.get('cpu',0):.0f} | {r.get('net_MBs',0):.1f} | {r.get('disk_w_s',0):.0f} | {r.get('disk_flush_s',0):.0f} |\n")
    f.write("\n**PUT stage means on the coordinator (ms)**:\n\n| Size | Variant | shard round | local cleanup | record round | total |\n|---|---|---|---|---|---|\n")
    for (v, s), t in sorted(breakdown.items(), key=lambda x: (x[0][1], x[0][0])):
        f.write(f"| {size_label(s)} | {v} | {t.get('shards_us',0)/1000:.2f} | {t.get('local_cleanup_us',0)/1000:.2f} | {t.get('replicate_us',0)/1000:.2f} | {t.get('total_us',0)/1000:.2f} |\n")
print("wrote", sorted(os.listdir(OUT)))
