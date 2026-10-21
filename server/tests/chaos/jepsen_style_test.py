#!/usr/bin/env python3
# Author: Tejus Chandrashekar
"""
Jepsen-style concurrent nemesis + workload test against a real local
WarpDrive cluster -- not a sequential scripted scenario:

  - One client thread per key, each the sole writer of its own key (so
    "the last write that completed before a read" is always unambiguous
    -- it was issued by exactly one thread's own sequential code, never a
    race between two independently-clocked connections writing the same
    key) but free to read any key at random. This produces real
    concurrent contention and a full operation history (invoke time,
    complete time, type, key, value, ok/fail) without an ambiguity the
    checker can't resolve.
  - An independent nemesis thread runs on its OWN random schedule the
    whole time -- it does not know or care what the clients are doing.
    Each tick it randomly kills a peer, restarts a previously-killed one,
    or flips a byte in an on-disk shard file.
  - After the run, a checker walks the recorded history per key and
    flags two classes of real violation:
      1. CORRUPTION: a read returned bytes that were never written by
         any op in this run at all.
      2. STALE: a read returned a value strictly older than the last
         write that fully completed (real-time) before the read began
         -- i.e. the read went backward in time, past an update it
         should have seen. Reads that return a value from a write
         *concurrent* with them are NOT flagged (ordering among truly
         concurrent ops is legitimately ambiguous and is not what this
         checks).

This is not the real Jepsen/Knossos/Elle codebase (Clojure, needs
Docker-orchestrated nodes reached over SSH) -- it is the same
methodology (independent nemesis + concurrent workload + history +
post-hoc consistency checker), implemented directly against this
project's real local cluster. See docs/CHAOS-TESTING.md for how this was
run, what it found, and what it deliberately does not cover yet (real
network partitions -- this only kills/restarts whole processes on one
machine, which is a different and easier failure mode).

Usage:
    cargo build --release --bin warp_drive   # from server/
    python3 server/tests/chaos/jepsen_style_test.py
"""
import hashlib
import json
import os
import pathlib
import random
import shutil
import subprocess
import threading
import time

import boto3
from botocore.client import Config

REPO_ROOT = pathlib.Path(__file__).resolve().parents[3]
SERVER_DIR = REPO_ROOT / "server"
BIN = SERVER_DIR / "target" / "release" / "warp_drive"
LOG_CFG = SERVER_DIR / "server_log.yaml"
WORK_DIR = "/tmp/wd-jepsen"  # disposable scratch, not a repo artifact
HISTORY_OUT = "/tmp/jepsen_history.json"

N = 5
BASE_PORT = 9710
ACCESS_KEY = "AKIAIOSFODNN7EXAMPLE"
SECRET_KEY = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"
BUCKET = "jepsen-bucket"
KEYS = [f"k{i}" for i in range(5)]
DURATION_SEC = int(os.environ.get("JEPSEN_DURATION_SEC", "90"))
CLIENT_THREADS = len(KEYS)  # one thread per key -- each key has exactly one writer

PEERS = ",".join(f"http://127.0.0.1:{BASE_PORT+i}" for i in range(N))
procs = {}
history_lock = threading.Lock()
history = []  # list of dict events
stop_flag = threading.Event()
killed = set()
killed_lock = threading.Lock()


def node_dir(i):
    return os.path.join(WORK_DIR, f"node{i}")


def start_node(i):
    if not BIN.exists():
        raise RuntimeError(f"{BIN} not found -- build it first: (cd server && cargo build --release --bin warp_drive)")
    d = node_dir(i)
    os.makedirs(os.path.join(d, "storage"), exist_ok=True)
    shutil.copy(LOG_CFG, os.path.join(d, "server_log.yaml"))
    env = dict(os.environ)
    env.update({
        "WARPDRIVE_PORT": str(BASE_PORT + i),
        "WARPDRIVE_PEERS": PEERS,
        "WARPDRIVE_RS_K": "3",
        "WARPDRIVE_RS_M": "2",
        "WARPDRIVE_LOCATION_LOG": os.path.join(d, "location.log"),
        "WARPDRIVE_CONTENT_LOCATION_LOG": os.path.join(d, "content_location.log"),
        "WARPDRIVE_BUCKET_CONFIG_LOG": os.path.join(d, "bucket_config.log"),
        "WARPDRIVE_SHARD_META_LOG": os.path.join(d, "shard_meta.log"),
        "STORAGE_DIRECTORY": os.path.join(d, "storage"),
        "WARPDRIVE_ADMIN_ACCESS_KEY": ACCESS_KEY,
        "WARPDRIVE_ADMIN_SECRET_KEY": SECRET_KEY,
        "RUST_LOG": "warn",
    })
    out = open(os.path.join(d, "stdout.log"), "a")
    p = subprocess.Popen([str(BIN)], cwd=d, env=env, stdout=out, stderr=subprocess.STDOUT)
    procs[i] = p
    with killed_lock:
        killed.discard(i)


def kill_node(i):
    p = procs.get(i)
    if p and p.poll() is None:
        p.kill()
        try:
            p.wait(timeout=5)
        except Exception:
            pass
    with killed_lock:
        killed.add(i)


def wait_healthy(i, timeout=10):
    import urllib.request
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            urllib.request.urlopen(f"http://127.0.0.1:{BASE_PORT+i}/cluster/_internal/timing", timeout=1)
            return True
        except Exception:
            time.sleep(0.2)
    return False


def start_all():
    shutil.rmtree(WORK_DIR, ignore_errors=True)
    for i in range(N):
        start_node(i)
    for i in range(N):
        if not wait_healthy(i):
            raise RuntimeError(f"node{i} never became healthy")


def stop_all():
    for i in list(procs.keys()):
        kill_node(i)


def client_for():
    return boto3.client(
        's3', endpoint_url=f'http://127.0.0.1:{BASE_PORT}/cluster/s3',
        aws_access_key_id=ACCESS_KEY, aws_secret_access_key=SECRET_KEY,
        region_name='us-east-1',
        config=Config(signature_version='s3v4', s3={'addressing_style': 'path'},
                      retries={'max_attempts': 1}, connect_timeout=3, read_timeout=5),
    )


def log_event(ev):
    with history_lock:
        history.append(ev)


# ---------------- Client workload threads ----------------

def client_worker(thread_id):
    c = client_for()
    own_key = KEYS[thread_id % len(KEYS)]
    seq = 0
    while not stop_flag.is_set():
        if random.random() < 0.6:
            seq += 1
            value = f"T{thread_id}-S{seq}-{random.randint(0, 10**9)}".encode()
            vid = hashlib.sha256(value).hexdigest()[:16]
            t0 = time.time()
            try:
                c.put_object(Bucket=BUCKET, Key=own_key, Body=value)
                ok = True
            except Exception:
                ok = False
            t1 = time.time()
            log_event({"type": "write", "key": own_key, "value": value, "vid": vid,
                       "t_invoke": t0, "t_complete": t1, "ok": ok, "thread": thread_id})
        else:
            key = random.choice(KEYS)
            t0 = time.time()
            try:
                body = c.get_object(Bucket=BUCKET, Key=key)['Body'].read()
                ok = True
            except Exception:
                body = None
                ok = False
            t1 = time.time()
            log_event({"type": "read", "key": key, "value": body, "vid": None,
                       "t_invoke": t0, "t_complete": t1, "ok": ok, "thread": thread_id})
        time.sleep(random.uniform(0.01, 0.05))


# ---------------- Nemesis thread ----------------

def corrupt_random_shard():
    i = random.randint(0, N - 1)
    d = os.path.join(node_dir(i), "storage", "admin")
    if not os.path.isdir(d):
        return None
    candidates = [fn for fn in os.listdir(d) if BUCKET in fn]
    if not candidates:
        return None
    path = os.path.join(d, candidates[0])
    try:
        with open(path, "r+b") as f:
            data = f.read()
            if len(data) < 32:
                return None
            pos = random.randint(0, len(data) - 1)
            f.seek(pos)
            f.write(bytes([data[pos] ^ 0xFF]))
        return f"corrupted 1 byte in node{i} storage at offset {pos}"
    except Exception as e:
        return f"corrupt attempt failed: {e}"


def nemesis_worker():
    while not stop_flag.is_set():
        time.sleep(random.uniform(1.0, 4.0))
        with killed_lock:
            currently_down = list(killed)
            up = [i for i in range(1, N) if i not in killed]  # never touch node0 (entry point)
        action = random.choice(["kill", "restart", "corrupt"] if (up and currently_down) else
                                (["kill"] if up else ["restart"]))
        t0 = time.time()
        if action == "kill" and up:
            victim = random.choice(up)
            kill_node(victim)
            log_event({"type": "nemesis", "action": "kill", "target": victim, "t_invoke": t0, "t_complete": time.time()})
        elif action == "restart" and currently_down:
            victim = random.choice(currently_down)
            start_node(victim)
            healthy = wait_healthy(victim, timeout=10)
            log_event({"type": "nemesis", "action": "restart", "target": victim, "healthy": healthy,
                       "t_invoke": t0, "t_complete": time.time()})
        elif action == "corrupt":
            detail = corrupt_random_shard()
            log_event({"type": "nemesis", "action": "corrupt", "detail": detail, "t_invoke": t0, "t_complete": time.time()})


# ---------------- Checker ----------------

def check_history():
    writes = [h for h in history if h["type"] == "write"]
    reads = [h for h in history if h["type"] == "read"]
    nemesis_events = [h for h in history if h["type"] == "nemesis"]
    print(f"\n=== history: {len(history)} client ops + nemesis events ===")
    print(f"writes={len(writes)} ({sum(1 for w in writes if w['ok'])} ok) "
          f"reads={len(reads)} ({sum(1 for r in reads if r['ok'])} ok) nemesis_events={len(nemesis_events)}")
    for ev in nemesis_events:
        print(f"  nemesis: {ev['action']} target={ev.get('target')} detail={ev.get('detail', '')}")

    violations = []
    all_written_values = {w["value"] for w in writes if w["ok"]}

    for key in KEYS:
        key_writes = sorted([w for w in writes if w["key"] == key and w["ok"]], key=lambda w: w["t_complete"])
        key_reads = [r for r in reads if r["key"] == key and r["ok"]]
        for r in key_reads:
            val = r["value"]
            if val is None:
                continue
            if val not in all_written_values and val != f"init-{key}".encode():
                violations.append(("CORRUPTION", key, r, "value was never written by this run"))
                continue
            # last write that fully completed strictly before this read began
            prior = [w for w in key_writes if w["t_complete"] <= r["t_invoke"]]
            if not prior:
                continue
            last_completed_before = prior[-1]
            if val == last_completed_before["value"]:
                continue
            # ok if val matches a write that overlaps with the read's
            # interval (invoked before the read completed, and not
            # already superseded by last_completed_before -- i.e. at
            # least as new)
            overlapping_or_newer = [
                w for w in key_writes
                if w["t_invoke"] <= r["t_complete"] and w["t_complete"] >= last_completed_before["t_complete"] and w["value"] == val
            ]
            if overlapping_or_newer:
                continue
            violations.append(("STALE", key, r,
                                f"saw a value from before the last write completed-before this read "
                                f"(last_completed_before t_complete={last_completed_before['t_complete']:.3f})"))

    print(f"\n=== {len(violations)} consistency violations found ===")
    for kind, key, r, detail in violations:
        print(f"  [{kind}] key={key} read_by_thread={r['thread']} t_invoke={r['t_invoke']:.3f} t_complete={r['t_complete']:.3f}: {detail}")
    if not violations:
        print("\nNO CONSISTENCY VIOLATIONS: every successful read returned either the latest "
              "completed-before write's value, or a value from a write concurrent with it. "
              "No corruption, no stale/lost-update reads, across the full run with random "
              "peer kills/restarts/on-disk corruption happening concurrently.")
    return violations


def main():
    print(f"=== starting {N}-node RS(3,2) cluster for {DURATION_SEC}s Jepsen-style run ===")
    start_all()
    c0 = client_for()
    c0.create_bucket(Bucket=BUCKET)
    for k in KEYS:
        c0.put_object(Bucket=BUCKET, Key=k, Body=f"init-{k}".encode())

    threads = [threading.Thread(target=client_worker, args=(i,), daemon=True) for i in range(CLIENT_THREADS)]
    nem = threading.Thread(target=nemesis_worker, daemon=True)
    start_time = time.time()
    for t in threads:
        t.start()
    nem.start()

    try:
        while time.time() - start_time < DURATION_SEC:
            time.sleep(2)
            with history_lock:
                n_ops = len(history)
            with killed_lock:
                down = sorted(killed)
            print(f"  t={time.time()-start_time:5.1f}s ops={n_ops} down={down}")
    finally:
        stop_flag.set()
        for t in threads:
            t.join(timeout=10)
        nem.join(timeout=10)
        with killed_lock:
            down = list(killed)
        for i in down:
            start_node(i)
            wait_healthy(i)
        print("=== stopping cluster ===")
        stop_all()

    with open(HISTORY_OUT, "w") as f:
        json.dump([{k: (v.decode("utf-8", "replace") if isinstance(v, bytes) else v) for k, v in h.items()} for h in history], f)
    print(f"full history saved to {HISTORY_OUT}")

    violations = check_history()
    return 1 if violations else 0


if __name__ == "__main__":
    raise SystemExit(main())
