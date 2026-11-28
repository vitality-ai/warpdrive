#!/usr/bin/env python3
"""loader.py <out prefix> <endpoint> <seconds>: 16 threads PUT distinct objects (4 KB, 64 KB, 1 MB random bytes)
for <seconds>, one attempt each, while 8 threads GET random already-acked objects and compare SHA-256 (a wrong
body is a correctness failure, an error is unavailability; both counted per second in <out>-reads.csv). Writes <out>-acked.json (key -> SHA-256 of what a 200 acknowledged) and
<out>-failed.json (keys whose PUT failed or timed out). Then, with LOADER_CHECK=1, <out prefix> <endpoint>...:
reads every acked key from each endpoint (all must match) and every failed key (counts how many are visible)."""
import boto3, hashlib, json, os, sys, threading, time
from botocore.config import Config
from concurrent.futures import ThreadPoolExecutor
cfg = Config(retries={"max_attempts": 1}, connect_timeout=2, read_timeout=15, max_pool_connections=64)
def client(ep):
    return boto3.client("s3", endpoint_url=ep, aws_access_key_id="AKIAIOSFODNN7EXAMPLE",
                        aws_secret_access_key="wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY", region_name="us-east-1", config=cfg)
out, bucket = sys.argv[1], "fault"
if os.environ.get("LOADER_CHECK"):
    acked, failed = json.load(open(out + "-acked.json")), json.load(open(out + "-failed.json")); bad = 0
    for ep in sys.argv[2:]:
        c = client(ep)
        def get(k):
            try:
                return hashlib.sha256(c.get_object(Bucket=bucket, Key=k)["Body"].read()).hexdigest()
            except Exception as e:
                return "ERR " + str(e)[:120]
        with ThreadPoolExecutor(32) as ex:
            got = dict(zip(acked, ex.map(get, acked))); seen = list(ex.map(get, failed))
        wrong = [k for k in acked if got[k] != acked[k]]
        print(f"{ep} acked={len(acked)} acked_wrong={len(wrong)} {[(k, got[k]) for k in wrong[:2]]} failed={len(failed)} failed_visible={sum(1 for s in seen if not s.startswith('ERR'))}")
        bad += len(wrong)
    sys.exit(1 if bad else 0)
ep, secs = sys.argv[2], float(sys.argv[3]); s3 = client(ep)
try:
    s3.create_bucket(Bucket=bucket)
except Exception as e:
    print("create_bucket:", e)
acked, failed, lock, n = {}, [], threading.Lock(), [0]
start = time.time(); end = start + secs
reads = {}  # second -> [ok, wrong, error]
wrong_keys = []
def reader(w):
    import random
    rnd = random.Random(w)
    while time.time() < end:
        with lock:
            keys = list(acked) if acked else []
        if not keys:
            time.sleep(0.05); continue
        k = rnd.choice(keys); sec = int(time.time() - start)
        try:
            ok = hashlib.sha256(s3.get_object(Bucket=bucket, Key=k)["Body"].read()).hexdigest() == acked[k]; r = 0 if ok else 1
            if not ok:
                with lock: wrong_keys.append(k)
        except Exception:
            r = 2; time.sleep(0.2)
        with lock:
            reads.setdefault(sec, [0, 0, 0])[r] += 1
def worker(w):
    i = 0
    while time.time() < end:
        size = (4096, 65536, 1048576)[i % 3]; k = f"w{w}-{i}"; i += 1; b = os.urandom(size)
        try:
            s3.put_object(Bucket=bucket, Key=k, Body=b)
            with lock: acked[k] = hashlib.sha256(b).hexdigest()
        except Exception:
            with lock: failed.append(k)
            time.sleep(0.2)
ts = [threading.Thread(target=worker, args=(w,)) for w in range(16)] + [threading.Thread(target=reader, args=(w,)) for w in range(8)]
[t.start() for t in ts]; [t.join() for t in ts]
json.dump(acked, open(out + "-acked.json", "w")); json.dump(failed, open(out + "-failed.json", "w"))
with open(out + "-reads.csv", "w") as f:
    f.write("second,ok,wrong,error\n"); [f.write(f"{t},{v[0]},{v[1]},{v[2]}\n") for t, v in sorted(reads.items())]
tot = [sum(v[i] for v in reads.values()) for i in range(3)]
print(f"acked={len(acked)} failed={len(failed)} reads_ok={tot[0]} reads_wrong={tot[1]} {wrong_keys[:3]} reads_error={tot[2]}")
