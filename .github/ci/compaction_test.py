#!/usr/bin/env python3
"""compaction_test.py <warp_drive binary> <run dir>: compaction frees disk on a real cluster (RFC 6).

Starts the CI cluster (.github/ci/start_cluster.sh: 5 nodes, RS(3,2)) with short grace periods and 1 MB segments, then:

1. Writes 300 objects (4 KB to 300 KB), overwrites half, deletes 60, deletes an old version in a versioned bucket,
   completes a multipart upload with a part uploaded twice and a part left out, and aborts another upload.
2. Records each node's segment bytes on disk (allocated blocks, so punched holes count as freed).
3. Runs the gc/ passes on every node while a writer and four readers keep going.
4. Fails unless: every node freed bytes, no request failed, every surviving object reads back byte for byte, a third
   pass finds nothing left, the audit sweep finds nothing left to free, and after restarting every node every object
   still reads back.

The per-node bytes before and after go to the log and, in GitHub Actions, to the job summary.
"""
import json, os, random, signal, socket, subprocess, sys, threading, time, urllib.request

import boto3
from botocore.config import Config

BIN, RUN = os.path.abspath(sys.argv[1]), os.path.abspath(sys.argv[2])
NODES, BASE = 5, 9710
HERE = os.path.dirname(os.path.abspath(__file__))
ENV = dict(os.environ,
           WARPDRIVE_SEGMENT_MB="1", WARPDRIVE_RECLAIM_GRACE_SECS="3", WARPDRIVE_RECLAIM_INTERVAL_SECS="999999",
           WARPDRIVE_RECLAIM_CONFIRM_SECS="2", WARPDRIVE_COMPACT_SEAL_SECS="2", WARPDRIVE_COMPACT_UNLINK_SECS="1",
           WARPDRIVE_COMPACT_MBPS="200", RS_K="3", RS_M="2", BASE_PORT=str(BASE))
AK, SK = os.environ["WARPDRIVE_ADMIN_ACCESS_KEY"], os.environ["WARPDRIVE_ADMIN_SECRET_KEY"]
failures = []


def check(ok, what):
    print(("PASS " if ok else "FAIL ") + what, flush=True)
    if not ok:
        failures.append(what)


def start():
    subprocess.run([f"{HERE}/start_cluster.sh", BIN, RUN], env=ENV, check=True)


def stop():
    for i in range(NODES):
        try:
            os.kill(int(open(f"{RUN}/node{i}/pid").read()), signal.SIGTERM)
        except (OSError, ValueError):
            pass
    # Every port (HTTP, gRPC +1000, TCP +2000) free before the cluster starts again.
    for _ in range(60):
        if not any(listening(BASE + i + d) for i in range(NODES) for d in (0, 1000, 2000)):
            return
        time.sleep(1)
    raise SystemExit("ports still busy after stopping the cluster")


def listening(port):
    with socket.socket() as s:
        return s.connect_ex(("127.0.0.1", port)) == 0


def client():
    return boto3.client("s3", endpoint_url=f"http://127.0.0.1:{BASE}", aws_access_key_id=AK, aws_secret_access_key=SK,
                        region_name="us-east-1", config=Config(s3={"addressing_style": "path"}, max_pool_connections=32))


def disk():
    """Each node's segment bytes, as allocated blocks."""
    out = []
    for i in range(NODES):
        n = 0
        for root, _, files in os.walk(f"{RUN}/node{i}/storage"):
            n += sum(os.stat(os.path.join(root, f)).st_blocks * 512 for f in files if f.endswith(".seg"))
        out.append(n)
    return out


def post(i, path):
    req = urllib.request.Request(f"http://127.0.0.1:{BASE + i}{path}", method="POST")
    return json.loads(urllib.request.urlopen(req, timeout=600).read())


def body(k, g, n):
    return (f"{k}-{g}-".encode() * (n // 8 + 1))[:n]


def get(c, b, k, v):
    return c.get_object(Bucket=b, Key=k, **({"VersionId": v} if v else {}))["Body"].read()


start()
c = client()
random.seed(11)
want = {}
c.create_bucket(Bucket="gcb")
sizes = {f"o{i}": random.choice([4096, 20_000, 100_000, 300_000]) for i in range(300)}
for k, n in sizes.items():
    c.put_object(Bucket="gcb", Key=k, Body=body(k, 0, n))
    want[("gcb", k, None)] = body(k, 0, n)
keys = list(sizes)
for k in keys[:150]:
    c.put_object(Bucket="gcb", Key=k, Body=body(k, 1, sizes[k]))
    want[("gcb", k, None)] = body(k, 1, sizes[k])
for k in keys[150:210]:
    c.delete_object(Bucket="gcb", Key=k)
    del want[("gcb", k, None)]
c.create_bucket(Bucket="gcv")
c.put_bucket_versioning(Bucket="gcv", VersioningConfiguration={"Status": "Enabled"})
v1 = c.put_object(Bucket="gcv", Key="x", Body=body("x", 1, 200_000))["VersionId"]
v2 = c.put_object(Bucket="gcv", Key="x", Body=body("x", 2, 200_000))["VersionId"]
c.delete_object(Bucket="gcv", Key="x", VersionId=v1)
want[("gcv", "x", v2)] = body("x", 2, 200_000)
P = 5 * 1024 * 1024
u = c.create_multipart_upload(Bucket="gcb", Key="mp")["UploadId"]
e1 = c.upload_part(Bucket="gcb", Key="mp", UploadId=u, PartNumber=1, Body=bytes([1]) * P)["ETag"]
c.upload_part(Bucket="gcb", Key="mp", UploadId=u, PartNumber=2, Body=bytes([9]) * P)
e2 = c.upload_part(Bucket="gcb", Key="mp", UploadId=u, PartNumber=2, Body=bytes([2]) * P)["ETag"]
c.upload_part(Bucket="gcb", Key="mp", UploadId=u, PartNumber=3, Body=bytes([3]) * P)
c.complete_multipart_upload(Bucket="gcb", Key="mp", UploadId=u,
                            MultipartUpload={"Parts": [{"PartNumber": 1, "ETag": e1}, {"PartNumber": 2, "ETag": e2}]})
want[("gcb", "mp", None)] = bytes([1]) * P + bytes([2]) * P
a = c.create_multipart_upload(Bucket="gcb", Key="ab")["UploadId"]
for n in (1, 2):
    c.upload_part(Bucket="gcb", Key="ab", UploadId=a, PartNumber=n, Body=bytes([7]) * P)
c.abort_multipart_upload(Bucket="gcb", Key="ab", UploadId=a)

before = disk()
live = sum(len(v) for v in want.values())
time.sleep(4)  # past the grace period

stop_load, errs, extra = False, [], {}


def writer():
    i = 0
    while not stop_load:
        k, b = f"during{i}", body(f"during{i}", 0, 30_000)
        try:
            c.put_object(Bucket="gcb", Key=k, Body=b)
            extra[("gcb", k, None)] = b
        except Exception as e:  # noqa: BLE001 - counted, reported below
            errs.append(f"put {k}: {type(e).__name__}")
        i += 1


def reader():
    ks = list(want)
    while not stop_load:
        t = random.choice(ks)
        try:
            if get(c, *t) != want[t]:
                errs.append(f"get {t}: wrong bytes")
        except Exception as e:  # noqa: BLE001
            errs.append(f"get {t}: {type(e).__name__}")


ts = [threading.Thread(target=writer)] + [threading.Thread(target=reader) for _ in range(4)]
[t.start() for t in ts]
passes = [post(i, "/cluster/_internal/gc/run") for i in range(NODES)]
time.sleep(2)
[post(i, "/cluster/_internal/gc/run") for i in range(NODES)]
stop_load = True
[t.join() for t in ts]
after = disk()

rows = [f"| node {i} | {b0 / 1e6:.1f} MB | {b1 / 1e6:.1f} MB | {(b0 - b1) / 1e6:.1f} MB ({100 * (b0 - b1) // max(b0, 1)}%) | "
        f"{g['shards']} | {g['compact']['segments_compacted']} | {g['compact']['bytes_punched'] / 1e6:.1f} MB |"
        for i, (b0, b1, g) in enumerate(zip(before, after, passes))]
table = "\n".join(["| Node | Segments before | After | Freed | Dead shards | Segments compacted | Punched |",
                   "|---|---|---|---|---|---|---|"] + rows)
print(f"live data {live / 1e6:.1f} MB, RS(3,2) on {NODES} nodes\n{table}", flush=True)
if os.environ.get("GITHUB_STEP_SUMMARY"):
    with open(os.environ["GITHUB_STEP_SUMMARY"], "a") as f:
        f.write(f"## Compaction frees disk\n\nLive data {live / 1e6:.1f} MB, RS(3,2) on {NODES} nodes.\n\n{table}\n")

check(all(b1 < b0 for b0, b1 in zip(before, after)), f"every node freed segment bytes ({sum(before) - sum(after)} in all)")
check(sum(before) - sum(after) > sum(before) // 5, "at least a fifth of the segment bytes freed")
check(not errs, f"no request failed while freeing ({len(errs)} failed: {errs[:3]})")
again = [post(i, "/cluster/_internal/gc/run")["rows"] for i in range(NODES)]
check(again == [0] * NODES, f"a third gc/ pass finds nothing left ({again})")
want.update(extra)
bad = [t for t, v in want.items() if get(c, *t) != v]
check(not bad, f"{len(want) - len(bad)}/{len(want)} objects read back exactly ({bad[:3]})")
time.sleep(4)
audit = [post(i, "/cluster/_internal/reclaim/report") for i in range(NODES)]
left = [x["reclaimable_superseded"] + x["reclaimable_confirmed"] for x in audit]
check(left == [0] * NODES, f"the audit sweep finds nothing left to free ({left})")

stop()
start()
c = client()
bad = [t for t, v in want.items() if get(c, *t) != v]
check(not bad, f"after restarting every node: {len(want) - len(bad)}/{len(want)} objects read back exactly ({bad[:3]})")
stop()
if failures:
    sys.exit(f"{len(failures)} check(s) failed")
print("compaction test passed")
