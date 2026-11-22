#!/usr/bin/env python3
"""Soak canary: keeps its own objects with unique contents on the cluster and checks every byte.
canary.py <endpoint> <seconds> <out dir>. Writes events.csv (unix_s, op, ms, ok, detail) and verdict.json."""
import boto3, json, os, random, sys, threading, time, hashlib
from botocore.config import Config
url, secs, out = sys.argv[1], int(sys.argv[2]), sys.argv[3]
os.makedirs(out, exist_ok=True)
c = boto3.client("s3", endpoint_url=url, aws_access_key_id="AKIAIOSFODNN7EXAMPLE", aws_secret_access_key="wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
                 region_name="us-east-1", config=Config(s3={"addressing_style": "path"}, max_pool_connections=16, retries={"max_attempts": 1}))
B = "canary"
try: c.create_bucket(Bucket=B)
except Exception: pass
want, lock, ev = {}, threading.Lock(), open(f"{out}/events.csv", "w")
bad = []
def body(k, g, n):
    seed = hashlib.sha256(f"{k}:{g}".encode()).digest()
    return (seed * (n // 32 + 1))[:n]
def log(op, t0, ok, detail=""):
    with lock: ev.write(f"{time.time():.3f},{op},{(time.time()-t0)*1000:.1f},{int(ok)},{detail}\n")
def worker(wid):
    r = random.Random(wid); gen = 0; end = time.time() + secs
    while time.time() < end:
        k = f"w{wid}-{r.randrange(250)}"; op = r.random(); t0 = time.time()
        try:
            if op < 0.40:
                gen += 1; n = r.choice([1000, 4096, 50_000, 300_000, 2_000_000]); b = body(k, gen, n)
                c.put_object(Bucket=B, Key=k, Body=b)
                with lock: want[k] = (gen, n)
                log("put", t0, True)
            elif op < 0.80:
                with lock: w = want.get(k)
                try:
                    got = c.get_object(Bucket=B, Key=k)["Body"].read()
                    ok = w is not None and got == body(k, *w)
                    if not ok: bad.append(f"{k}: wrong bytes (want gen {w})")
                    log("get", t0, ok, "" if ok else "mismatch")
                except c.exceptions.NoSuchKey:
                    ok = w is None
                    if not ok: bad.append(f"{k}: missing (want gen {w})")
                    log("get", t0, ok, "" if ok else "missing")
            elif op < 0.90:
                c.delete_object(Bucket=B, Key=k)
                with lock: want.pop(k, None)
                log("delete", t0, True)
            elif op < 0.95:
                gen += 1; P = 5 * 1024 * 1024; parts = r.choice([2, 3])
                u = c.create_multipart_upload(Bucket=B, Key=k)["UploadId"]
                es = [{"PartNumber": i, "ETag": c.upload_part(Bucket=B, Key=k, UploadId=u, PartNumber=i, Body=body(k + f"p{i}", gen, P))["ETag"]} for i in range(1, parts + 1)]
                if r.random() < 0.2:
                    c.abort_multipart_upload(Bucket=B, Key=k, UploadId=u); log("mpu_abort", t0, True)
                else:
                    c.complete_multipart_upload(Bucket=B, Key=k, UploadId=u, MultipartUpload={"Parts": es})
                    with lock: want[k] = ("mpu", gen, parts)
                    log("mpu", t0, True)
            else:
                pre = f"w{wid}-"; keys = {o["Key"] for p in c.get_paginator("list_objects_v2").paginate(Bucket=B, Prefix=pre) for o in p.get("Contents", [])}
                with lock: exp = {kk for kk in want if kk.startswith(pre)}
                ok = keys == exp
                if not ok: bad.append(f"list {pre}: {len(keys ^ exp)} keys differ")
                log("list", t0, ok, "" if ok else f"diff {len(keys ^ exp)}")
        except Exception as e:
            log(op < 0.4 and "put" or "other", t0, False, f"{type(e).__name__}: {str(e)[:120]}".replace(",", ";"))
_body = body
def body(k, *w):  # mpu bodies: concatenated parts
    if len(w) == 3 and w[0] == "mpu":
        _, g, parts = w; return b"".join(_body(k + f"p{i}", g, 5 * 1024 * 1024) for i in range(1, parts + 1))
    return _body(k, *w)
ts = [threading.Thread(target=worker, args=(i,)) for i in range(8)]
[t.start() for t in ts]; [t.join() for t in ts]
# final full check of every object the canary believes exists
final_bad = []
for k, w in sorted(want.items()):
    try:
        if c.get_object(Bucket=B, Key=k)["Body"].read() != body(k, *w): final_bad.append(k)
    except Exception as e: final_bad.append(f"{k} {type(e).__name__}")
ev.close()
json.dump({"objects": len(want), "final_mismatches": final_bad[:50], "final_mismatch_count": len(final_bad), "live_check_failures": bad[:50], "live_check_failure_count": len(bad)}, open(f"{out}/verdict.json", "w"), indent=1)
print(json.dumps({"objects": len(want), "final_mismatch_count": len(final_bad), "live_check_failure_count": len(bad)}))
