#!/usr/bin/env python3
"""
Multi-tenant cold-start scaling benchmark for WarpDrive characterization.

Protocol per tenant count T:
  1. Stop all endpoints
  2. Wipe local layer files (forces cold start)
  3. Restart pageserver
  4. Apply Phase 9 config (4MB / 5s timeout / evict@10s)
  5. Reset WarpDrive metrics to 0
  6. Start T endpoints  (startup GETs captured here)
  7. Run sysbench 120s across all T endpoints in parallel
  8. Capture final metrics + delta/image GET breakdown
  9. Save structured results to logs/scaling/T<N>/

Usage:
  python3 run_scaling.py              # runs T=1,4,8,16
  python3 run_scaling.py --tenants 4 8
  python3 run_scaling.py --tenants 1  --dry-run
"""

import argparse
import json
import os
import re
import signal
import subprocess
import sys
import time
from concurrent.futures import ThreadPoolExecutor, as_completed
from datetime import datetime, timezone
from pathlib import Path

# ── config ───────────────────────────────────────────────────────────────────

NEON_ROOT     = Path("/home/nash/cj/warpdrive/neon-src")
NEON_LOCAL    = NEON_ROOT / "target/release/neon_local"
PAGESERVER_BIN= NEON_ROOT / "target/release/pageserver"
PAGESERVER_DIR= NEON_ROOT / ".neon/pageserver_1"
PAGESERVER_LOG= Path("/tmp/warpdrive_pageserver.log")
TENANT_ID     = "48619806dd4d362a965bc701199c9ee4"
TENANT_DIR    = PAGESERVER_DIR / "tenants" / TENANT_ID
WARPDRIVE_URL = f"http://{os.environ.get('STORAGE_BACKEND_HOST', 'localhost')}:9710"
WARPDRIVE_AUTH= ("adminkey", "adminsecretkey123456")
SYSBENCH_DIR  = Path("/home/nash/cj/warpdrive/sysbench-tpcc")
RESULTS_ROOT  = Path("/home/nash/cj/warpdrive/warpdrive/docs/benchmarks/logs/scaling_v2")
MONITOR_SCRIPT= Path("/home/nash/cj/warpdrive/warpdrive/docs/benchmarks/monitor_layer_downloads.py")

SYSBENCH_TIME    = 120   # seconds
SYSBENCH_SCALE   = 10
SYSBENCH_TABLES  = 1
WARMUP_WAIT      = 5     # seconds after starting endpoints before sysbench

# Phase 9 config — 4MB checkpoint, 5s timeout, evict after 10s idle
PHASE9_CONF = {
    "checkpoint_distance": 4194304,
    "checkpoint_timeout": "5s",
    "eviction_policy": {
        "kind": "LayerAccessThreshold",
        "period": "5s",
        "threshold": "10s",
    },
}

# Endpoint roster — ordered by preference; take first T.
# ep-7 and ep-8 have no pgdata (storcon orphans), kept at end as last resort.
# Each entry: (endpoint_name, pg_port)
ENDPOINTS = [
    ("main",  55432),
    ("ep-2",  55451),
    ("ep-3",  55435),
    ("ep-4",  55436),
    ("ep-5",  55465),
    ("ep-6",  55466),
    ("ep-a",  55480),
    ("ep-b",  55481),
    ("ep-c",  55482),
    ("ep-d",  55483),
    ("ep-e",  55484),
    ("ep-f",  55485),
    ("ep-g",  55486),
    ("ep-h",  55487),
    ("ep-i",  55488),
    ("ep-j",  55489),
    ("ep-1",  55441),
    ("ep-7",  55467),
    ("ep-8",  55468),
]

# ── helpers ──────────────────────────────────────────────────────────────────

def log(msg):
    ts = datetime.now(timezone.utc).strftime("%H:%M:%S")
    print(f"[{ts}] {msg}", flush=True)

def run(cmd, **kwargs):
    return subprocess.run(cmd, shell=True, text=True, capture_output=True, **kwargs)

def check(cmd, label):
    r = run(cmd)
    if r.returncode != 0:
        log(f"FAIL {label}: {r.stderr.strip()[:200]}")
        return False
    return True

def warpdrive_get(path):
    import urllib.request, base64
    creds = base64.b64encode(b"adminkey:adminsecretkey123456").decode()
    req = urllib.request.Request(f"{WARPDRIVE_URL}{path}",
                                  headers={"Authorization": f"Basic {creds}"})
    with urllib.request.urlopen(req, timeout=10) as r:
        return json.loads(r.read())

def warpdrive_post(path):
    import urllib.request, base64
    creds = base64.b64encode(b"adminkey:adminsecretkey123456").decode()
    req = urllib.request.Request(f"{WARPDRIVE_URL}{path}", method="POST",
                                  headers={"Authorization": f"Basic {creds}",
                                           "Content-Length": "0"})
    with urllib.request.urlopen(req, timeout=10) as r:
        return r.read()

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
    """Read new log lines since start_pos and count delta/image downloads."""
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
    # Graceful stop via neon_local (clears storcon state)
    for ep, _ in ENDPOINTS:
        ep_dir = NEON_ROOT / ".neon/endpoints" / ep
        if ep_dir.exists():
            run(f"{NEON_LOCAL} endpoint stop {ep} 2>/dev/null || true")
    time.sleep(2)
    # Force-kill any remaining compute_ctl processes (neon_local stop is unreliable
    # when endpoint.json HTTP ports have been changed since the process started)
    r = run("ps aux")
    for line in r.stdout.splitlines():
        if "compute_ctl" in line and "external-http-port" in line and "grep" not in line:
            parts = line.split()
            try:
                os.kill(int(parts[1]), signal.SIGKILL)
            except Exception:
                pass
    # Force-kill any remaining postgres endpoint processes and clean up pidfiles
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
    # Wait for OS to release TCP ports
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
                # layer files: start with hex digits, no extension
                if re.match(r'^[0-9A-Fa-f]', f.name):
                    f.unlink()
                    deleted += 1
    log(f"  Deleted {deleted} local layer files")

def restart_pageserver(dry_run=False):
    log("Restarting pageserver...")
    # Kill existing
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
    # Wait until it's accepting connections
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

def reset_metrics():
    log("Resetting WarpDrive metrics...")
    warpdrive_post("/_admin/metrics/reset")
    m = warpdrive_get("/_admin/metrics")
    assert m["ops"]["get"] == 0 and m["ops"]["put"] == 0, "Metrics not reset"
    log("  Metrics at zero")

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
    """Run sysbench against one endpoint. Returns parsed result dict."""
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
            result["txn"] = int(m.group(1))
            result["tps"] = float(m.group(2))
        m = re.search(r"avg:\s+([0-9.]+)", line)
        if m:
            result["lat_avg"] = float(m.group(1))
        m = re.search(r"95th percentile:\s+([0-9.]+)", line)
        if m:
            result["lat_p95"] = float(m.group(1))
        m = re.search(r"queries performed:", line)
        if m:
            pass
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
        reset_metrics()

    log_pos_before_start = pageserver_log_pos()

    start_endpoints(endpoints, dry_run)

    if not dry_run:
        log(f"Waiting {WARMUP_WAIT}s for endpoints to settle...")
        time.sleep(WARMUP_WAIT)

    # capture startup GETs (before sysbench)
    startup_metrics = warpdrive_get("/_admin/metrics") if not dry_run else {"ops": {}}
    log_pos_after_start = pageserver_log_pos()
    startup_downloads, startup_events = collect_layer_downloads(PAGESERVER_LOG, log_pos_before_start)
    log(f"  Startup GETs: {startup_metrics.get('ops',{}).get('get',0)} "
        f"(delta={startup_downloads['delta']} image={startup_downloads['image']})")

    # run sysbench in parallel across all endpoints
    log(f"Running sysbench {SYSBENCH_TIME}s across {T} endpoint(s)...")
    bench_start = time.monotonic()
    bench_start_epoch = time.time()
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
    bench_elapsed = time.monotonic() - bench_start
    bench_end_epoch = time.time()

    # final metrics
    final_metrics = warpdrive_get("/_admin/metrics") if not dry_run else {"ops": {}, "estimated_cost_usd": 0}
    bench_downloads, bench_events = collect_layer_downloads(PAGESERVER_LOG, log_pos_after_start)
    log(f"  Benchmark GETs: {final_metrics.get('ops',{}).get('get',0) - startup_metrics.get('ops',{}).get('get',0)} "
        f"(delta={bench_downloads['delta']} image={bench_downloads['image']})")

    # aggregate sysbench
    total_tps = sum(r["tps"] for r in sysbench_results)
    total_txn = sum(r["txn"] for r in sysbench_results)
    avg_lat   = sum(r["lat_avg"] for r in sysbench_results) / len(sysbench_results)
    p95_lat   = max(r["lat_p95"] for r in sysbench_results)

    ops = final_metrics.get("ops", {})
    result = {
        "timestamp":        datetime.now(timezone.utc).isoformat(),
        "T":                T,
        "bench_start_epoch": bench_start_epoch,
        "bench_end_epoch":   bench_end_epoch,
        "endpoints":        [{"name": ep, "port": port} for ep, port in endpoints],
        "config":           PHASE9_CONF,
        "sysbench": {
            "time_s":       SYSBENCH_TIME,
            "scale":        SYSBENCH_SCALE,
            "tables":       SYSBENCH_TABLES,
            "threads_per_endpoint": 4,
            "total_tps":    round(total_tps, 2),
            "total_txn":    total_txn,
            "avg_lat_ms":   round(avg_lat, 1),
            "p95_lat_ms":   round(p95_lat, 1),
            "per_endpoint": sysbench_results,
        },
        "warpdrive": {
            "startup": {
                "ops":       startup_metrics.get("ops", {}),
                "get_delta": startup_downloads["delta"],
                "get_image": startup_downloads["image"],
            },
            "benchmark": {
                "ops":       ops,
                "get_delta": bench_downloads["delta"],
                "get_image": bench_downloads["image"],
            },
            "total": {
                "put":          ops.get("put", 0),
                "get":          ops.get("get", 0),
                "get_delta":    startup_downloads["delta"] + bench_downloads["delta"],
                "get_image":    startup_downloads["image"] + bench_downloads["image"],
                "delete_objects": ops.get("delete_objects", 0),
                "estimated_cost_usd": final_metrics.get("estimated_cost_usd", 0),
            },
        },
        "layer_events":     startup_events + bench_events,
    }

    out_path = out_dir / "result.json"
    out_path.write_text(json.dumps(result, indent=2))
    log(f"\n  DONE T={T}")
    log(f"  TPS:    {total_tps:.1f} total  ({total_tps/T:.1f} per tenant)")
    log(f"  Lat:    {avg_lat:.0f}ms avg  {p95_lat:.0f}ms p95")
    log(f"  PUTs:   {ops.get('put',0)}  GETs: {ops.get('get',0)}  DEL_OBJ: {ops.get('delete_objects',0)}")
    log(f"  Cost:   ${final_metrics.get('estimated_cost_usd',0):.6f}")
    log(f"  Saved:  {out_path}")
    return result

# ── main ─────────────────────────────────────────────────────────────────────

SANITY_MIN_TPS  = 1.0    # TPS below this = something is wrong
SANITY_MIN_PUTS = 1      # PUTs below this = checkpointing not firing
SANITY_MIN_GETS = 1      # GETs below this after cold start = cold start not working

def sanity_check(result):
    """Return list of (severity, message) issues. severity: 'FAIL' | 'WARN'."""
    issues = []
    s = result["sysbench"]
    w = result["warpdrive"]["total"]

    if s["total_tps"] < SANITY_MIN_TPS:
        issues.append(("FAIL", f"TPS={s['total_tps']:.1f} — sysbench may not have connected"))
    if s["total_txn"] == 0:
        issues.append(("FAIL", "0 transactions — sysbench produced no output"))
    if w["put"] < SANITY_MIN_PUTS:
        issues.append(("FAIL", f"PUTs={w['put']} — checkpoint not firing (config not applied?)"))
    if w["get"] < SANITY_MIN_GETS:
        issues.append(("WARN", f"GETs={w['get']} — expected >0 on cold start; layers may not have been wiped"))
    if w["get"] > 0 and w["get_delta"] + w["get_image"] == 0:
        issues.append(("WARN", "GETs recorded but none classified delta/image — pageserver log may have been missed"))
    if s["avg_lat_ms"] > 5000:
        issues.append(("WARN", f"avg latency {s['avg_lat_ms']:.0f}ms — very high, check for endpoint startup delay"))
    return issues

def verify_and_continue(result, dry_run=False):
    """Print sanity check and ask user whether to proceed."""
    if dry_run:
        return True

    T = result["T"]
    s = result["sysbench"]
    w = result["warpdrive"]["total"]

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
    if has_fail:
        prompt = "  FAILs detected. [s]kip this T and continue / [a]bort / [c]ontinue anyway? "
    else:
        prompt = "  Press Enter to continue to next run, or [a]bort: "

    try:
        ans = input(prompt).strip().lower()
    except EOFError:
        ans = ""

    if ans == "a":
        log("Aborted by user.")
        sys.exit(0)
    if ans == "s":
        log(f"Skipping T={T} result (not added to summary).")
        return False
    return True

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--tenants", nargs="+", type=int, default=[16])
    ap.add_argument("--dry-run", action="store_true")
    ap.add_argument("--no-verify", action="store_true",
                    help="Skip interactive verification between runs (for unattended overnight runs)")
    args = ap.parse_args()

    max_available = len(ENDPOINTS)
    tenant_counts = [T for T in args.tenants if T <= max_available]
    if len(tenant_counts) < len(args.tenants):
        skipped = [T for T in args.tenants if T > max_available]
        log(f"WARNING: skipping {skipped} — only {max_available} endpoints available")

    log(f"Scaling benchmark: T = {tenant_counts}")
    log(f"Config: 4MB checkpoint / 5s timeout / evict@10s")
    log(f"Sysbench: {SYSBENCH_TIME}s / 4 threads per endpoint / scale={SYSBENCH_SCALE}")
    if args.dry_run:
        log("DRY RUN — no actual changes")

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

    # summary table
    log(f"\n{'='*60}")
    log("SUMMARY")
    log(f"{'='*60}")
    log(f"{'T':>4}  {'TPS':>8}  {'TPS/T':>7}  {'Avg lat':>8}  {'GETs':>6}  {'PUTs':>6}  {'DEL_OBJ':>8}  {'Cost':>12}")
    for r in all_results:
        w = r["warpdrive"]["total"]
        s = r["sysbench"]
        log(f"{r['T']:>4}  {s['total_tps']:>8.1f}  {s['total_tps']/r['T']:>7.1f}  "
            f"{s['avg_lat_ms']:>7.0f}ms  {w['get']:>6}  {w['put']:>6}  "
            f"{w['delete_objects']:>8}  ${w['estimated_cost_usd']:>11.6f}")

    summary_path = RESULTS_ROOT / "summary.json"
    summary_path.write_text(json.dumps(all_results, indent=2))
    log(f"\nFull results: {summary_path}")

if __name__ == "__main__":
    main()
