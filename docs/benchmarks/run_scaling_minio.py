#!/usr/bin/env python3
"""
Multi-tenant cold-start scaling benchmark — MinIO baseline.

Same protocol as run_scaling.py / run_scaling_slab.py but backed by MinIO
instead of WarpDrive.  S3 op counts are read from the pageserver's own
Prometheus metrics endpoint (/metrics) rather than a WarpDrive admin API.

Usage:
  python3 run_scaling_minio.py              # runs T=1,4,8,16
  python3 run_scaling_minio.py --tenants 1
  python3 run_scaling_minio.py --tenants 1 --no-verify
"""

import argparse
import json
import os
import re
import signal
import subprocess
import sys
import time
import urllib.request
from concurrent.futures import ThreadPoolExecutor, as_completed
from datetime import datetime, timezone
from pathlib import Path

# ── config ───────────────────────────────────────────────────────────────────

NEON_ROOT     = Path("/home/nash/cj/warpdrive/neon-src")
NEON_LOCAL    = NEON_ROOT / "target/release/neon_local"
PAGESERVER_BIN= NEON_ROOT / "target/release/pageserver"
PAGESERVER_DIR= NEON_ROOT / ".neon/pageserver_1"
PAGESERVER_LOG= Path("/tmp/warpdrive_pageserver.log")
PAGESERVER_METRICS = "http://127.0.0.1:9898/metrics"
TENANT_ID     = "bf15ffc04f5f086e83febfff46d6774c"
TENANT_DIR    = PAGESERVER_DIR / "tenants" / TENANT_ID
SYSBENCH_DIR  = Path("/home/nash/cj/warpdrive/sysbench-tpcc")
RESULTS_ROOT  = Path("/home/nash/cj/warpdrive/warpdrive/docs/benchmarks/logs/scaling_minio")

SYSBENCH_TIME    = 120
SYSBENCH_SCALE   = 10
SYSBENCH_TABLES  = 1
WARMUP_WAIT      = 5

# S3 pricing (standard AWS us-east-1)
PUT_COST_PER_OP = 5.0    / 1_000_000   # $0.005 / 1000 PUTs  = $0.000005/PUT
GET_COST_PER_OP = 0.40   / 1_000_000   # $0.0004 / 1000 GETs = $0.0000004/GET

PHASE9_CONF = {
    "checkpoint_distance": 4194304,
    "checkpoint_timeout": "5s",
    "eviction_policy": {
        "kind": "LayerAccessThreshold",
        "period": "5s",
        "threshold": "10s",
    },
}

ENDPOINTS = [
    ("main",  55432),
    ("ep-2",  55451),
    ("ep-5",  55465),
    ("ep-6",  55466),
    ("ep-a",  55480),
    ("ep-b",  55481),
    ("ep-e",  55484),
    ("ep-f",  55485),
    ("ep-g",  55486),
    ("ep-h",  55487),
    ("ep-k",  55490),
    ("ep-l",  55492),
    ("ep-m",  55494),
    ("ep-n",  55496),
    ("ep-o",  55498),
    ("ep-p",  55500),
]

# ── helpers ──────────────────────────────────────────────────────────────────

def log(msg):
    ts = datetime.now(timezone.utc).strftime("%H:%M:%S")
    print(f"[{ts}] {msg}", flush=True)

def run(cmd, **kwargs):
    return subprocess.run(cmd, shell=True, text=True, capture_output=True, **kwargs)

# ── pageserver S3 metrics ─────────────────────────────────────────────────────

def _parse_metrics(text):
    """Parse Prometheus text format, return {metric_name: value} for S3 counters."""
    vals = {}
    for line in text.splitlines():
        if line.startswith("#"):
            continue
        m = re.match(r'remote_storage_s3_request_seconds_count\{request_type="(\w+)",result="ok"\}\s+([\d.]+)', line)
        if m:
            vals[m.group(1)] = float(m.group(2))
        m2 = re.match(r'remote_storage_s3_deleted_objects_total\s+([\d.]+)', line)
        if m2:
            vals["deleted_objects"] = float(m2.group(1))
    return vals

def get_ps_metrics():
    """Return dict with get/put/delete counts from pageserver Prometheus metrics."""
    try:
        with urllib.request.urlopen(PAGESERVER_METRICS, timeout=10) as r:
            raw = r.read().decode()
    except Exception as e:
        log(f"  WARN pageserver metrics fetch failed: {e}")
        return {"get": 0, "put": 0, "delete_objects": 0}
    v = _parse_metrics(raw)
    return {
        "get":            int(v.get("get_object", 0)),
        "put":            int(v.get("put_object", 0)),
        "delete_objects": int(v.get("deleted_objects", 0)),
    }

def metrics_delta(before, after):
    return {k: after[k] - before[k] for k in before}

def estimate_cost(ops):
    puts = ops.get("put", 0)
    gets = ops.get("get", 0)
    return round(puts * PUT_COST_PER_OP + gets * GET_COST_PER_OP, 6)

# ── layer download classifier (from pageserver log) ──────────────────────────

DOWNLOAD_RE = re.compile(
    r"get_or_maybe_download\{layer=(?P<layer>[^\}]+)\}.*?downloading on-demand"
)

def classify_layer(layer_name):
    if "__" not in layer_name:
        return "unknown"
    lsn_part = layer_name.split("__", 1)[1]
    lsn_part = re.sub(r"-[0-9a-fA-F]{8}$", "", lsn_part)
    parts = lsn_part.split("-")
    return "delta" if len(parts) == 2 else "image" if len(parts) == 1 else "unknown"

def collect_layer_downloads(log_path, start_pos):
    counts = {"delta": 0, "image": 0, "unknown": 0}
    events = []
    try:
        with open(log_path) as f:
            f.seek(start_pos)
            for line in f:
                m = DOWNLOAD_RE.search(line)
                if not m:
                    continue
                kind = classify_layer(m.group("layer"))
                counts[kind] += 1
                events.append({"kind": kind, "layer": m.group("layer")})
    except Exception:
        pass
    return counts, events

def pageserver_log_pos():
    try:
        return PAGESERVER_LOG.stat().st_size
    except Exception:
        return 0

# ── protocol steps ────────────────────────────────────────────────────────────

def stop_all_endpoints(dry_run=False):
    log("Stopping all endpoints...")
    if dry_run:
        return
    for ep, _ in ENDPOINTS:
        ep_dir = NEON_ROOT / ".neon/endpoints" / ep
        if ep_dir.exists():
            run(f"{NEON_LOCAL} endpoint stop {ep} 2>/dev/null || true")
    time.sleep(2)
    r = run("ps aux")
    for line in r.stdout.splitlines():
        if "compute_ctl" in line and "external-http-port" in line and "grep" not in line:
            parts = line.split()
            try:
                os.kill(int(parts[1]), signal.SIGKILL)
            except Exception:
                pass
    endpoints_dir = NEON_ROOT / ".neon/endpoints"
    for pidfile in sorted(endpoints_dir.glob("*/pgdata/postmaster.pid")):
        try:
            pid = int(pidfile.read_text().split()[0])
            os.kill(pid, signal.SIGKILL)
        except Exception:
            pass
        try:
            pidfile.unlink()
        except Exception:
            pass
    time.sleep(4)

def wipe_local_layers(dry_run=False):
    log("Wiping local layer files...")
    if dry_run:
        log("  [dry-run] would delete layer files under " + str(TENANT_DIR))
        return
    deleted = 0
    for timeline_dir in (TENANT_DIR / "timelines").glob("*/"):
        for f in timeline_dir.glob("*"):
            if f.is_file() and not f.suffix == ".json" and not f.name.startswith("."):
                if re.match(r'^[0-9A-Fa-f]', f.name):
                    f.unlink()
                    deleted += 1
    log(f"  Deleted {deleted} local layer files")

def restart_pageserver(dry_run=False):
    log("Restarting pageserver...")
    run("pkill -TERM -f 'pageserver -D' 2>/dev/null || true")
    time.sleep(2)
    run("pkill -KILL -f 'pageserver -D' 2>/dev/null || true")
    time.sleep(1)
    if dry_run:
        log("  [dry-run] would restart pageserver")
        return
    proc = subprocess.Popen(
        [str(PAGESERVER_BIN), "-D", str(PAGESERVER_DIR)],
        stdout=open(PAGESERVER_LOG, "a"),
        stderr=subprocess.STDOUT,
    )
    log(f"  Pageserver PID {proc.pid}")
    for _ in range(30):
        time.sleep(1)
        r = run("curl -sf http://127.0.0.1:9898/v1/status")
        if r.returncode == 0:
            log("  Pageserver ready")
            return
    raise RuntimeError("Pageserver did not start within 30s")

def wait_tenant_active(timeout=60):
    log(f"Waiting for tenant {TENANT_ID[:8]}... to become Active...")
    for _ in range(timeout):
        time.sleep(1)
        r = run(f"curl -sf http://127.0.0.1:9898/v1/tenant/{TENANT_ID}")
        if r.returncode == 0:
            d = json.loads(r.stdout)
            if d.get("state", {}).get("slug") == "Active":
                log("  Tenant Active")
                return
    raise RuntimeError("Tenant did not become Active within timeout")

def apply_phase9_config():
    log("Applying Phase 9 config (4MB / 5s / evict@10s)...")
    gen_r = run(f"curl -sf http://127.0.0.1:9898/v1/tenant/{TENANT_ID}")
    gen = json.loads(gen_r.stdout).get("generation", 1)
    payload = json.dumps({
        "mode": "AttachedSingle",
        "generation": gen,
        "tenant_conf": PHASE9_CONF,
    })
    r = run(f"curl -sf -X PUT http://127.0.0.1:9898/v1/tenant/{TENANT_ID}/location_config "
            f"-H 'Content-Type: application/json' -d '{payload}'")
    if r.returncode != 0:
        log(f"  WARN location_config PUT failed: {r.stderr.strip()[:200]}")
    else:
        log("  Config applied")

def snapshot_metrics():
    """Take a snapshot of pageserver S3 counters (used as delta baseline)."""
    m = get_ps_metrics()
    log(f"  Metrics snapshot: GET={m['get']} PUT={m['put']} DEL={m['delete_objects']}")
    return m

def start_endpoints(endpoints, dry_run=False):
    log(f"Starting {len(endpoints)} endpoint(s): {[e for e,_ in endpoints]}")
    for ep, port in endpoints:
        if dry_run:
            log(f"  [dry-run] would start {ep} on :{port}")
            continue
        r = run(f"cd {NEON_ROOT} && ./target/release/neon_local endpoint start {ep} "
                f"--start-timeout 120s 2>&1")
        if r.returncode == 0:
            log(f"  {ep} started on :{port}")
        else:
            log(f"  {ep} failed: {r.stdout.strip()[-120:]}")

def run_sysbench_one(ep_name, port, out_path, dry_run=False):
    if dry_run:
        return {"endpoint": ep_name, "port": port, "tps": 0.0, "lat_avg": 0.0,
                "lat_p95": 0.0, "txn": 0, "queries": 0, "errors": 0}
    cmd = (
        f"cd {SYSBENCH_DIR} && sysbench tpcc.lua "
        f"--pgsql-host=127.0.0.1 --pgsql-port={port} "
        f"--pgsql-user=cloud_admin --pgsql-db=postgres "
        f"--threads=4 --tables={SYSBENCH_TABLES} --scale={SYSBENCH_SCALE} "
        f"--time={SYSBENCH_TIME} --db-driver=pgsql run"
    )
    r = run(cmd)
    out_path.write_text(r.stdout + r.stderr)
    return parse_sysbench(ep_name, port, r.stdout)

def parse_sysbench(ep_name, port, output):
    result = {"endpoint": ep_name, "port": port,
              "tps": 0.0, "lat_avg": 0.0, "lat_p95": 0.0,
              "txn": 0, "queries": 0, "errors": 0}
    for line in output.splitlines():
        line = line.strip()
        m = re.search(r"transactions:\s+(\d+)\s+\(([0-9.]+) per sec", line)
        if m:
            result["txn"] = int(m.group(1)); result["tps"] = float(m.group(2))
        m = re.search(r"avg:\s+([0-9.]+)", line)
        if m:
            result["lat_avg"] = float(m.group(1))
        m = re.search(r"95th percentile:\s+([0-9.]+)", line)
        if m:
            result["lat_p95"] = float(m.group(1))
        m = re.search(r"total:\s+(\d+)$", line)
        if m and result["queries"] == 0:
            result["queries"] = int(m.group(1))
        m = re.search(r"ignored errors:\s+(\d+)", line)
        if m:
            result["errors"] = int(m.group(1))
    return result

# ── single T run ──────────────────────────────────────────────────────────────

def run_one(T, dry_run=False):
    endpoints = ENDPOINTS[:T]
    out_dir = RESULTS_ROOT / f"T{T}"
    out_dir.mkdir(parents=True, exist_ok=True)

    log(f"\n{'='*60}")
    log(f"  STARTING RUN: T={T} tenants")
    log(f"  Output: {out_dir}")
    log(f"{'='*60}")

    stop_all_endpoints(dry_run)
    wipe_local_layers(dry_run)
    restart_pageserver(dry_run)

    if not dry_run:
        wait_tenant_active()
        apply_phase9_config()
        metrics_before = snapshot_metrics()
    else:
        metrics_before = {"get": 0, "put": 0, "delete_objects": 0}

    log_pos_before_start = pageserver_log_pos()

    start_endpoints(endpoints, dry_run)

    if not dry_run:
        log(f"Waiting {WARMUP_WAIT}s for endpoints to settle...")
        time.sleep(WARMUP_WAIT)

    startup_metrics_after = get_ps_metrics() if not dry_run else metrics_before
    log_pos_after_start = pageserver_log_pos()
    startup_downloads, startup_events = collect_layer_downloads(PAGESERVER_LOG, log_pos_before_start)
    startup_ops = metrics_delta(metrics_before, startup_metrics_after)
    log(f"  Startup GETs: {startup_ops['get']} "
        f"(delta={startup_downloads['delta']} image={startup_downloads['image']})")

    log(f"Running sysbench {SYSBENCH_TIME}s across {T} endpoint(s)...")
    sysbench_results = []
    with ThreadPoolExecutor(max_workers=T) as ex:
        futs = {
            ex.submit(run_sysbench_one, ep, port, out_dir / f"sysbench_{port}.out", dry_run): (ep, port)
            for ep, port in endpoints
        }
        for fut in as_completed(futs):
            r = fut.result()
            sysbench_results.append(r)
            log(f"  {r['endpoint']}:{r['port']}  {r['tps']:.1f} TPS  {r['lat_avg']:.0f}ms avg")

    final_metrics = get_ps_metrics() if not dry_run else metrics_before
    bench_ops = metrics_delta(startup_metrics_after, final_metrics)
    total_ops = metrics_delta(metrics_before, final_metrics)
    bench_downloads, bench_events = collect_layer_downloads(PAGESERVER_LOG, log_pos_after_start)
    log(f"  Benchmark GETs: {bench_ops['get']} "
        f"(delta={bench_downloads['delta']} image={bench_downloads['image']})")

    total_tps = sum(r["tps"] for r in sysbench_results)
    total_txn = sum(r["txn"] for r in sysbench_results)
    avg_lat   = sum(r["lat_avg"] for r in sysbench_results) / max(len(sysbench_results), 1)
    p95_lat   = max((r["lat_p95"] for r in sysbench_results), default=0)

    cost = estimate_cost(total_ops)
    result = {
        "timestamp":  datetime.now(timezone.utc).isoformat(),
        "T":          T,
        "endpoints":  [{"name": ep, "port": port} for ep, port in endpoints],
        "config":     PHASE9_CONF,
        "sysbench": {
            "time_s": SYSBENCH_TIME, "scale": SYSBENCH_SCALE,
            "tables": SYSBENCH_TABLES, "threads_per_endpoint": 4,
            "total_tps": round(total_tps, 2), "total_txn": total_txn,
            "avg_lat_ms": round(avg_lat, 1), "p95_lat_ms": round(p95_lat, 1),
            "per_endpoint": sysbench_results,
        },
        "minio": {
            "startup": {"ops": startup_ops, "get_delta": startup_downloads["delta"], "get_image": startup_downloads["image"]},
            "benchmark": {"ops": bench_ops, "get_delta": bench_downloads["delta"], "get_image": bench_downloads["image"]},
            "total": {
                "put":            total_ops["put"],
                "get":            total_ops["get"],
                "get_delta":      startup_downloads["delta"] + bench_downloads["delta"],
                "get_image":      startup_downloads["image"] + bench_downloads["image"],
                "delete_objects": total_ops["delete_objects"],
                "estimated_cost_usd": cost,
            },
        },
        "layer_events": startup_events + bench_events,
    }

    out_path = out_dir / "result.json"
    out_path.write_text(json.dumps(result, indent=2))

    log(f"\n  DONE T={T}")
    log(f"  TPS:    {total_tps:.1f} total  ({total_tps/T:.1f} per tenant)")
    log(f"  Lat:    {avg_lat:.0f}ms avg  {p95_lat:.0f}ms p95")
    log(f"  PUTs:   {total_ops['put']}  GETs: {total_ops['get']}  DEL_OBJ: {total_ops['delete_objects']}")
    log(f"  Cost:   ${cost:.6f}")
    log(f"  Saved:  {out_path}")
    return result

# ── sanity + verify ───────────────────────────────────────────────────────────

def sanity_check(result):
    issues = []
    s = result["sysbench"]
    w = result["minio"]["total"]
    if s["total_tps"] < 1.0:
        issues.append(("FAIL", f"TPS={s['total_tps']:.1f} — sysbench may not have connected"))
    if s["total_txn"] == 0:
        issues.append(("FAIL", "0 transactions"))
    if w["put"] < 1:
        issues.append(("FAIL", f"PUTs={w['put']} — checkpoint not firing"))
    if w["get"] < 1:
        issues.append(("WARN", f"GETs={w['get']} — expected >0 on cold start"))
    if s["avg_lat_ms"] > 5000:
        issues.append(("WARN", f"avg latency {s['avg_lat_ms']:.0f}ms — very high"))
    return issues

def verify_and_continue(result, dry_run=False):
    if dry_run:
        return True
    T = result["T"]
    s = result["sysbench"]
    w = result["minio"]["total"]
    print()
    print(f"  ┌─ VERIFY T={T} ──────────────────────────────────────")
    print(f"  │  TPS      {s['total_tps']:>8.1f} total  ({s['total_tps']/T:.1f}/tenant)")
    print(f"  │  Latency  {s['avg_lat_ms']:>7.0f}ms avg  {s['p95_lat_ms']:.0f}ms p95")
    print(f"  │  PUTs     {w['put']:>8}   GETs {w['get']}  (delta={w['get_delta']} image={w['get_image']})")
    print(f"  │  DEL_OBJ  {w['delete_objects']:>8}   Cost ${w['estimated_cost_usd']:.6f}")
    issues = sanity_check(result)
    if issues:
        for sev, msg in issues:
            marker = "  │  ✗ FAIL" if sev == "FAIL" else "  │  ⚠ WARN"
            print(f"{marker}  {msg}")
    else:
        print(f"  │  ✓ All checks passed")
    print(f"  └──────────────────────────────────────────────────")
    print()
    has_fail = any(sev == "FAIL" for sev, _ in issues)
    prompt = "  FAILs detected. [s]kip / [a]bort / [c]ontinue? " if has_fail else \
             "  Press Enter to continue to next run, or [a]bort: "
    try:
        ans = input(prompt).strip().lower()
    except EOFError:
        ans = ""
    if ans == "a":
        log("Aborted by user."); sys.exit(0)
    if ans == "s":
        log(f"Skipping T={T}."); return False
    return True

# ── main ─────────────────────────────────────────────────────────────────────

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--tenants", nargs="+", type=int, default=[1, 4, 8, 16])
    ap.add_argument("--dry-run", action="store_true")
    ap.add_argument("--no-verify", action="store_true")
    args = ap.parse_args()

    tenant_counts = [T for T in args.tenants if T <= len(ENDPOINTS)]
    log(f"Scaling benchmark (MinIO baseline): T = {tenant_counts}")
    log(f"Config: 4MB checkpoint / 5s timeout / evict@10s")
    log(f"Sysbench: {SYSBENCH_TIME}s / 4 threads per endpoint / scale={SYSBENCH_SCALE}")

    RESULTS_ROOT.mkdir(parents=True, exist_ok=True)
    all_results = []
    for T in tenant_counts:
        result = run_one(T, dry_run=args.dry_run)
        keep = True
        if not args.no_verify:
            keep = verify_and_continue(result, dry_run=args.dry_run)
        if keep:
            all_results.append(result)
        if T != tenant_counts[-1]:
            log("Cooling down 10s before next run...")
            time.sleep(10)

    log(f"\n{'='*60}")
    log("SUMMARY")
    log(f"{'='*60}")
    log(f"{'T':>4}  {'TPS':>8}  {'TPS/T':>7}  {'Avg lat':>8}  {'GETs':>6}  {'PUTs':>6}  {'DEL_OBJ':>8}  {'Cost':>12}")
    for r in all_results:
        w = r["minio"]["total"]
        s = r["sysbench"]
        log(f"{r['T']:>4}  {s['total_tps']:>8.1f}  {s['total_tps']/r['T']:>7.1f}  "
            f"{s['avg_lat_ms']:>7.0f}ms  {w['get']:>6}  {w['put']:>6}  "
            f"{w['delete_objects']:>8}  ${w['estimated_cost_usd']:>11.6f}")

    summary_path = RESULTS_ROOT / "summary.json"
    summary_path.write_text(json.dumps(all_results, indent=2))
    log(f"\nFull results: {summary_path}")

if __name__ == "__main__":
    main()
