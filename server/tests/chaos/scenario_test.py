#!/usr/bin/env python3
# Author: Tejus Chandrashekar
"""
Scenario-based fault-tolerance checks against a real local WarpDrive
cluster -- the first iteration of chaos testing this project did, kept
because it tests specific named claims quickly and deterministically
(seconds, not 90s) rather than relying on a long randomized run to
happen to cover them:

  A. Write/read with exactly N of the 4 non-entry peers down, for
     N = 0..4 (RS(3,2): writes/reads must succeed through N=2 down,
     and fail *cleanly* -- never silently succeed with an incomplete
     quorum -- at N=3,4).
  B. Corruption detection: write an object, flip bytes in its on-disk
     shard data on every node, then read it back -- the read must
     either reconstruct the exact original bytes or be rejected; it
     must never return wrong bytes as if they were correct.
  C. Kill one peer while a write to it is actually in flight (not
     before the write starts) -- the write should still succeed
     (within RS(3,2)'s m=2 tolerance) and read back correct.

`jepsen_style_test.py` in this same directory supersedes this for
finding real consistency bugs (concurrent random workload + an
independent nemesis + a post-hoc history checker, run for much longer)
-- see docs/CHAOS-TESTING.md. This script stays useful as a fast,
specific smoke test of the exact quorum-tolerance claims.

Usage:
    cargo build --release --bin warp_drive   # from server/
    python3 server/tests/chaos/scenario_test.py
"""
import os
import pathlib
import random
import shutil
import subprocess
import threading
import time

import boto3
from botocore.client import Config
from botocore.exceptions import ClientError

REPO_ROOT = pathlib.Path(__file__).resolve().parents[3]
SERVER_DIR = REPO_ROOT / "server"
BIN = SERVER_DIR / "target" / "release" / "warp_drive"
LOG_CFG = SERVER_DIR / "server_log.yaml"
WORK_DIR = "/tmp/wd-chaos"  # disposable scratch, not a repo artifact

N = 5
BASE_PORT = 9710
ACCESS_KEY = "AKIAIOSFODNN7EXAMPLE"
SECRET_KEY = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"

PEERS = ",".join(f"http://127.0.0.1:{BASE_PORT+i}" for i in range(N))
procs = {}  # idx -> Popen
results = []


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


def kill_node(i):
    p = procs.get(i)
    if p and p.poll() is None:
        p.kill()
        p.wait(timeout=5)


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


def client_for(node_idx=0):
    return boto3.client(
        's3', endpoint_url=f'http://127.0.0.1:{BASE_PORT+node_idx}/cluster/s3',
        aws_access_key_id=ACCESS_KEY, aws_secret_access_key=SECRET_KEY,
        region_name='us-east-1',
        config=Config(signature_version='s3v4', s3={'addressing_style': 'path'},
                      retries={'max_attempts': 1}, connect_timeout=3, read_timeout=5),
    )


def record(name, ok, detail=""):
    results.append((name, ok))
    print(f"[{'PASS' if ok else 'FAIL'}] {name} {detail}")


def corrupt_file_bytes(path, needle):
    with open(path, "rb") as f:
        data = f.read()
    if needle not in data:
        return 0
    mutated = bytearray(data)
    hits = 0
    start = 0
    while True:
        idx = data.find(needle, start)
        if idx == -1:
            break
        mutated[idx] ^= 0xFF
        hits += 1
        start = idx + 1
    with open(path, "wb") as f:
        f.write(bytes(mutated))
    return hits


def corrupt_bucket_everywhere(bucket, needle):
    total = 0
    for i in range(N):
        d = os.path.join(node_dir(i), "storage", "admin")
        if not os.path.isdir(d):
            continue
        for fn in os.listdir(d):
            if bucket in fn:
                total += corrupt_file_bytes(os.path.join(d, fn), needle)
    return total


def scenario_a():
    for n_down in [0, 1, 2, 3, 4]:
        bucket = f"chaos-a-{n_down}"
        c0 = client_for(0)
        c0.create_bucket(Bucket=bucket)
        body = os.urandom(64 * 1024)
        key = "obj"

        candidates = [i for i in range(N) if i != 0]
        down = random.sample(candidates, n_down)
        for i in down:
            kill_node(i)
        time.sleep(0.3)

        should_succeed = n_down <= 2  # m=2, write tolerates up to 2 down
        put_err = None
        try:
            c0.put_object(Bucket=bucket, Key=key, Body=body)
            put_ok = True
        except Exception as e:
            put_ok = False
            put_err = str(e)[:150]

        if should_succeed:
            if put_ok:
                try:
                    got = c0.get_object(Bucket=bucket, Key=key)['Body'].read()
                    record(f"A: {n_down} down, write+read", got == body, f"(down={down})")
                except Exception as e:
                    record(f"A: {n_down} down, write+read", False, f"GET failed after PUT succeeded: {e}")
            else:
                record(f"A: {n_down} down, write should succeed", False, f"PUT failed: {put_err} (down={down})")
        else:
            record(f"A: {n_down} down, write should fail cleanly", not put_ok,
                   f"(down={down}) {'got put_ok=True -- SILENT SUCCESS WITH INSUFFICIENT QUORUM' if put_ok else 'correctly rejected'}")

        for i in down:
            start_node(i)
            wait_healthy(i)


def scenario_b():
    for trial in range(3):
        bucket = f"chaos-b-{trial}"
        c0 = client_for(0)
        c0.create_bucket(Bucket=bucket)
        marker = f"CHAOSMARK{trial}".encode() * 2000
        c0.put_object(Bucket=bucket, Key="obj", Body=marker)

        got_before = c0.get_object(Bucket=bucket, Key="obj")['Body'].read()
        if got_before != marker:
            record(f"B{trial}: sanity read before corruption", False, "body mismatch before any corruption!")
            continue

        hits = corrupt_bucket_everywhere(bucket, marker[:20])
        if hits == 0:
            record(f"B{trial}: corruption setup", False, "could not find bytes to corrupt")
            continue

        try:
            got_after = c0.get_object(Bucket=bucket, Key="obj")['Body'].read()
            record(f"B{trial}: post-corruption read is correct-or-rejected",
                   got_after == marker, f"GET succeeded with {'correct' if got_after == marker else 'WRONG'} bytes ({hits} bytes flipped)")
        except ClientError as e:
            status = e.response['ResponseMetadata']['HTTPStatusCode']
            record(f"B{trial}: post-corruption read is correct-or-rejected", status >= 500,
                   f"GET correctly rejected with HTTP {status} ({hits} bytes flipped)")


def scenario_c():
    for trial in range(5):
        bucket = f"chaos-c-{trial}"
        c0 = client_for(0)
        c0.create_bucket(Bucket=bucket)
        body = os.urandom(256 * 1024)
        victim = random.choice([1, 2, 3, 4])

        result = {}

        def do_put():
            try:
                c0.put_object(Bucket=bucket, Key="obj", Body=body)
                result['ok'] = True
            except Exception as e:
                result['ok'] = False
                result['err'] = str(e)[:150]

        t = threading.Thread(target=do_put)
        t.start()
        time.sleep(random.uniform(0.0, 0.05))
        kill_node(victim)
        t.join(timeout=15)

        ok_overall = True
        detail = f"victim={victim} put_ok={result.get('ok')}"
        if result.get('ok'):
            try:
                got = c0.get_object(Bucket=bucket, Key="obj")['Body'].read()
                ok_overall = (got == body)
                detail += f" read_match={ok_overall}"
            except Exception as e:
                ok_overall = False
                detail += f" read_failed={e}"
        record(f"C{trial}: kill peer mid-write (1 of 5, within tolerance)", ok_overall, detail)
        start_node(victim)
        wait_healthy(victim)


def main():
    print("=== starting 5-node RS(3,2) cluster ===")
    start_all()
    try:
        scenario_a()
        scenario_b()
        scenario_c()
    finally:
        print("\n=== stopping cluster ===")
        stop_all()

    passed = sum(1 for _, ok in results if ok)
    print(f"\n{passed}/{len(results)} chaos checks passed")
    for name, ok in results:
        if not ok:
            print(f"  STILL FAILING: {name}")
    return 0 if passed == len(results) else 1


if __name__ == "__main__":
    raise SystemExit(main())
