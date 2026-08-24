#!/usr/bin/env python3
"""
Stdlib-only CPU / memory / network sampler.

Samples /proc/stat, /proc/meminfo, /proc/net/dev at a fixed interval and
writes one CSV row per sample. No external dependencies (no psutil, no
sysstat/dstat) so it runs on a bare Ubuntu image without extra apt installs.

Usage:
  python3 resource_monitor.py --out resource_neon_T4.csv --interval 1 &
  MONITOR_PID=$!
  ... run the actual benchmark ...
  kill -TERM $MONITOR_PID
"""

import argparse
import csv
import signal
import sys
import time

RUNNING = True


def _stop(signum, frame):
    global RUNNING
    RUNNING = False


def read_cpu_totals():
    with open("/proc/stat") as f:
        line = f.readline()
    parts = line.split()
    # user nice system idle iowait irq softirq steal guest guest_nice
    fields = [int(x) for x in parts[1:11]]
    idle = fields[3] + fields[4]  # idle + iowait
    total = sum(fields)
    return total, idle


def read_mem():
    vals = {}
    with open("/proc/meminfo") as f:
        for line in f:
            k, v = line.split(":", 1)
            vals[k.strip()] = int(v.strip().split()[0])  # kB
    total_kb = vals.get("MemTotal", 0)
    avail_kb = vals.get("MemAvailable", 0)
    used_kb = total_kb - avail_kb
    return total_kb, used_kb


def read_net():
    rx = tx = 0
    with open("/proc/net/dev") as f:
        lines = f.readlines()[2:]
    for line in lines:
        iface, rest = line.split(":", 1)
        iface = iface.strip()
        if iface == "lo":
            continue
        fields = rest.split()
        rx += int(fields[0])
        tx += int(fields[8])
    return rx, tx


def main():
    global RUNNING
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", required=True)
    ap.add_argument("--interval", type=float, default=1.0)
    args = ap.parse_args()

    signal.signal(signal.SIGTERM, _stop)
    signal.signal(signal.SIGINT, _stop)

    prev_total, prev_idle = read_cpu_totals()
    prev_rx, prev_tx = read_net()
    prev_t = time.time()

    with open(args.out, "w", newline="") as f:
        w = csv.writer(f)
        w.writerow([
            "timestamp", "cpu_pct", "mem_used_mb", "mem_total_mb",
            "net_rx_kbps", "net_tx_kbps",
        ])
        f.flush()
        while RUNNING:
            time.sleep(args.interval)
            now = time.time()
            total, idle = read_cpu_totals()
            mem_total_kb, mem_used_kb = read_mem()
            rx, tx = read_net()

            dt = max(now - prev_t, 1e-6)
            d_total = total - prev_total
            d_idle = idle - prev_idle
            cpu_pct = 0.0 if d_total <= 0 else 100.0 * (1 - d_idle / d_total)
            rx_kbps = (rx - prev_rx) / 1024.0 / dt
            tx_kbps = (tx - prev_tx) / 1024.0 / dt

            w.writerow([
                f"{now:.3f}", f"{cpu_pct:.1f}",
                f"{mem_used_kb / 1024.0:.1f}", f"{mem_total_kb / 1024.0:.1f}",
                f"{rx_kbps:.1f}", f"{tx_kbps:.1f}",
            ])
            f.flush()

            prev_total, prev_idle, prev_rx, prev_tx, prev_t = total, idle, rx, tx, now


if __name__ == "__main__":
    main()
