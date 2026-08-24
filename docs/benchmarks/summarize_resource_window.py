#!/usr/bin/env python3
"""
Slice a continuous resource_monitor.py CSV to the [start_epoch, end_epoch]
window recorded in a result.json (bench_start_epoch / bench_end_epoch), and
print/save avg+max CPU/mem/net for that window.

Usage:
  python3 summarize_resource_window.py --csv resource_neon_full.csv \
      --start 1787397346.5 --end 1787397466.5 --out resource_neon_T4.json
"""

import argparse
import csv
import json


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--csv", required=True)
    ap.add_argument("--start", type=float, required=True)
    ap.add_argument("--end", type=float, required=True)
    ap.add_argument("--out")
    args = ap.parse_args()

    rows = []
    with open(args.csv) as f:
        for row in csv.DictReader(f):
            t = float(row["timestamp"])
            if args.start <= t <= args.end:
                rows.append(row)

    if not rows:
        summary = {"samples": 0}
    else:
        cpu = [float(r["cpu_pct"]) for r in rows]
        mem = [float(r["mem_used_mb"]) for r in rows]
        rx = [float(r["net_rx_kbps"]) for r in rows]
        tx = [float(r["net_tx_kbps"]) for r in rows]
        summary = {
            "samples": len(rows),
            "cpu_pct_avg": round(sum(cpu) / len(cpu), 1),
            "cpu_pct_max": round(max(cpu), 1),
            "mem_used_mb_avg": round(sum(mem) / len(mem), 1),
            "mem_used_mb_max": round(max(mem), 1),
            "net_rx_kbps_avg": round(sum(rx) / len(rx), 1),
            "net_rx_kbps_max": round(max(rx), 1),
            "net_tx_kbps_avg": round(sum(tx) / len(tx), 1),
            "net_tx_kbps_max": round(max(tx), 1),
        }

    print(json.dumps(summary, indent=2))
    if args.out:
        with open(args.out, "w") as f:
            json.dump(summary, f, indent=2)


if __name__ == "__main__":
    main()
