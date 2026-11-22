#!/usr/bin/env python3
"""Summarize compaction A/B runs: analyze_comp.py <run dir>... -> summary.csv, timeseries.csv in the cwd.

Each arm dir (<size>_<arm>) holds bench.csv, per_second.csv, node<i>.csv
(unix_s, rss_kb, seg_kb, segs, meta_kb, cpu_pct, nvme_pct every 5 s) and node<i>.log.
Disk = segment bytes on disk (punched holes excluded). Live = keys x size / k per node."""
import csv, glob, os, re, sys, statistics as st

K = 2  # these GCP runs are RS(2,1)
KEYS = {"4096": 100_000, "1048576": 1_000, "268435456": 20}
STAT_RE = re.compile(r"(\w+): (\d+)")

def node_rows(d):
    out = []
    for f in sorted(glob.glob(f"{d}/node[0-9].csv")):
        rows = [list(map(int, r)) for r in csv.reader(open(f)) if len(r) == 7 and r[0].isdigit()]
        out.append(rows)
    return out

def compactor_totals(d):
    tot = {}
    for f in glob.glob(f"{d}/node[0-9].log"):
        for line in open(f, errors="replace"):
            m = re.search(r"CompactStats \{([^}]*)\}", line)
            if not m:
                continue
            for k, v in STAT_RE.findall(m.group(1)):
                tot[k] = tot.get(k, 0) + int(v)
    return tot

def main(runs):
    summary, series = [], []
    for run in runs:
        for d in sorted(glob.glob(f"{run}/*_*")):
            if not os.path.isdir(d) or not os.path.exists(f"{d}/bench.csv"):
                continue
            size, arm = os.path.basename(d).split("_", 1)
            bench = {r[0]: r for r in csv.reader(open(f"{d}/bench.csv"))}
            put, get = bench.get("put"), bench.get("get")
            if not put or not get or not glob.glob(f"{d}/node0.csv"):
                continue  # still running
            ps = [r for r in csv.DictReader(open(f"{d}/per_second.csv"))]
            put_secs = [r for r in ps if r["phase"].lower() == "put"]
            put_len = max(int(r["second"]) for r in put_secs) + 1 if put_secs else 0
            nodes = node_rows(d)
            t0 = min(n[0][0] for n in nodes if n)
            live_gb = KEYS[size] * int(size) / K / 1e9
            # per node, aligned on t0; disk = mean over nodes
            def at(sec):
                vals = []
                for n in nodes:
                    pts = [r for r in n if r[0] - t0 <= sec]
                    if pts:
                        vals.append(pts[-1][2] * 1024 / 1e9)
                return st.mean(vals) if vals else 0.0
            last = max(n[-1][0] for n in nodes) - t0
            peak = max(st.mean([r[2] for r in n if abs(r[0] - (t0 + s)) < 3] or [0]) * 1024 / 1e9 for n in nodes for s in range(0, last, 5)) if nodes else 0
            ct = compactor_totals(d)
            summary.append({
                "run": os.path.basename(run), "size": size, "arm": arm,
                "put_ops_s": float(put[5]), "put_p50_ms": int(put[7]) / 1000, "put_p99_ms": int(put[9]) / 1000, "put_errors": int(put[4]),
                "get_ops_s": float(get[5]), "get_p99_ms": int(get[9]) / 1000, "get_errors": int(get[4]),
                "live_gb_per_node": round(live_gb, 3),
                "disk_gb_end_of_writes": round(at(put_len), 3), "disk_gb_peak": round(peak, 3), "disk_gb_end": round(at(last), 3),
                "space_amp_end_of_writes": round(at(put_len) / live_gb, 2), "space_amp_end": round(at(last) / live_gb, 2),
                "cpu_pct_writes": round(st.mean(r[5] for n in nodes for r in n if r[0] - t0 <= put_len), 1),
                "dead_entries_dropped": ct.get("dead_entries_dropped", 0), "bytes_copied": ct.get("bytes_copied", 0),
                "bytes_punched": ct.get("bytes_punched", 0), "bytes_freed": ct.get("bytes_freed", 0),
            })
            for m in range(0, last // 60 + 1):
                sec = [r for r in put_secs if m * 60 <= int(r["second"]) < (m + 1) * 60]
                cpu = [r[5] for n in nodes for r in n if m * 60 <= r[0] - t0 < (m + 1) * 60]
                series.append({"run": os.path.basename(run), "size": size, "arm": arm, "minute": m,
                               "disk_gb": round(at(m * 60 + 59), 3), "live_gb": round(live_gb, 3),
                               "put_ops_s": round(sum(int(r["ops"]) for r in sec) / 60, 1) if sec else "",
                               "put_p99_ms": round(max(int(r["p99_us"]) for r in sec) / 1000, 1) if sec else "",
                               "cpu_pct": round(st.mean(cpu), 1) if cpu else ""})
    for name, rows in (("summary.csv", summary), ("timeseries.csv", series)):
        if rows:
            w = csv.DictWriter(open(name, "w"), fieldnames=list(rows[0]))
            w.writeheader(); w.writerows(rows)
    return summary

if __name__ == "__main__":
    for r in main(sys.argv[1:]):
        print(r)
