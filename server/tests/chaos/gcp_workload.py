#!/usr/bin/env python3
# Author: Tejus Chandrashekar
# Deployed and launched by gcp_orchestrator.py -- runs ON a GCP node
# (node-0), hitting the real cluster over its own localhost:9710. Same
# single-writer-per-key design as the local jepsen_style_test.py (each
# thread exclusively owns one key for writes, reads any key), adapted for
# a real multi-machine cluster. Not meant to be run standalone.
import hashlib
import json
import os
import random
import sys
import threading
import time

import boto3
from botocore.client import Config

ACCESS_KEY = "AKIAIOSFODNN7EXAMPLE"
SECRET_KEY = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"
BUCKET = "jepsen-gcp-bucket"
KEYS = [f"k{i}" for i in range(5)]
DURATION_SEC = int(sys.argv[1]) if len(sys.argv) > 1 else 180
CLIENT_THREADS = len(KEYS)
HISTORY_OUT = os.path.expanduser("~/gcp_jepsen_history.json")

history_lock = threading.Lock()
history = []
stop_flag = threading.Event()


def client_for():
    return boto3.client(
        's3', endpoint_url='http://localhost:9710/cluster/s3',
        aws_access_key_id=ACCESS_KEY, aws_secret_access_key=SECRET_KEY,
        region_name='us-east-1',
        config=Config(signature_version='s3v4', s3={'addressing_style': 'path'},
                      retries={'max_attempts': 1}, connect_timeout=3, read_timeout=5),
    )


def log_event(ev):
    with history_lock:
        history.append(ev)


def client_worker(thread_id):
    c = client_for()
    own_key = KEYS[thread_id % len(KEYS)]
    seq = 0
    while not stop_flag.is_set():
        if random.random() < 0.6:
            seq += 1
            value = f"T{thread_id}-S{seq}-{random.randint(0, 10**9)}".encode()
            t0 = time.time()
            try:
                c.put_object(Bucket=BUCKET, Key=own_key, Body=value)
                ok = True
            except Exception:
                ok = False
            t1 = time.time()
            log_event({"type": "write", "key": own_key, "value": value,
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
            log_event({"type": "read", "key": key, "value": body,
                       "t_invoke": t0, "t_complete": t1, "ok": ok, "thread": thread_id})
        time.sleep(random.uniform(0.02, 0.08))


def main():
    c0 = client_for()
    for attempt in range(10):
        try:
            c0.create_bucket(Bucket=BUCKET)
            break
        except Exception as e:
            print(f"create_bucket retry {attempt}: {e}")
            time.sleep(1)
    for k in KEYS:
        c0.put_object(Bucket=BUCKET, Key=k, Body=f"init-{k}".encode())

    threads = [threading.Thread(target=client_worker, args=(i,), daemon=True) for i in range(CLIENT_THREADS)]
    start_time = time.time()
    for t in threads:
        t.start()

    while time.time() - start_time < DURATION_SEC:
        time.sleep(5)
        with history_lock:
            n_ops = len(history)
        print(f"t={time.time()-start_time:.1f}s ops={n_ops}", flush=True)

    stop_flag.set()
    for t in threads:
        t.join(timeout=10)

    with open(HISTORY_OUT, "w") as f:
        json.dump([{k: (v.decode("utf-8", "replace") if isinstance(v, bytes) else v) for k, v in h.items()} for h in history], f)
    print(f"DONE history={len(history)} saved to {HISTORY_OUT}", flush=True)


if __name__ == "__main__":
    main()
