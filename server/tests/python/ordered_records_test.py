"""Ordered record writes, checked live on a local 3-node RS(2,1) cluster.

Start the cluster first:
    WARPDRIVE_BASE_PORT=19710 ./scripts/run_local_cluster.sh 2 1
then:
    python3 ordered_records_test.py ../../cluster-local


Checks, through S3 and through each node's record log:
  - PUT, overwrite, DELETE, PUT after DELETE end the same on every node
  - tagging applies to the live version, and every node agrees after
  - multipart complete and abort tombstone the part records on every node
  - an older record sent straight to a node is refused with 409
  - an update naming an overwritten version is refused with 412
"""
import json, os, sys, time
import boto3, requests
from botocore.config import Config

BASE = int(os.environ.get("BASE_PORT", "19710"))
NODES = [f"http://127.0.0.1:{BASE + i}" for i in range(3)]
RUN = sys.argv[1]  # server/cluster-local
failures = []


def check(name, cond, detail=""):
    print(("PASS " if cond else "FAIL ") + name + (f"  ({detail})" if detail and not cond else ""))
    if not cond:
        failures.append(name)


def s3(node=0):
    return boto3.client("s3", endpoint_url=f"{NODES[node]}/cluster/s3", aws_access_key_id="adminkey",
                        aws_secret_access_key="adminsecretkey123456", region_name="us-east-1",
                        config=Config(s3={"addressing_style": "path"}, retries={"max_attempts": 1}))


def entries(node, key_suffix):
    """Last log entry per record key on `node` whose record key ends with `key_suffix`."""
    out = {}
    path = os.path.join(RUN, f"node{node}", "cluster_location.log")
    for line in open(path):
        rk, entry = json.loads(line)
        if rk.endswith(":" + key_suffix) or rk.endswith(key_suffix):
            out[rk] = entry
    return out


def state(node, key):
    e = [v for k, v in entries(node, key).items() if k.endswith(":" + key)]
    if not e:
        return None
    e = e[-1]
    if "Live" in e:
        return ("live", e["Live"]["version"], e["Live"]["seq"], e["Live"].get("tags", []))
    return ("tomb", e["Tombstone"]["seq"])


B = "ordtest"
c = s3(0)
c.create_bucket(Bucket=B)

# PUT, overwrite through another node, read back from the third.
c.put_object(Bucket=B, Key="a", Body=b"one" * 1000)
s3(1).put_object(Bucket=B, Key="a", Body=b"two" * 1000)
body = s3(2).get_object(Bucket=B, Key="a")["Body"].read()
check("overwrite through another node reads back the new bytes", body == b"two" * 1000)
st = [state(n, "a") for n in range(3)]
check("every node holds the same live version after an overwrite", len({s[:3] for s in st}) == 1 and st[0][0] == "live", st)

# DELETE leaves the same tombstone everywhere.
s3(2).delete_object(Bucket=B, Key="a")
st = [state(n, "a") for n in range(3)]
check("every node holds the same tombstone after DELETE", len(set(st)) == 1 and st[0][0] == "tomb", st)
try:
    c.get_object(Bucket=B, Key="a")
    check("GET after DELETE is 404", False)
except c.exceptions.NoSuchKey:
    check("GET after DELETE is 404", True)

# PUT after DELETE creates it again.
s3(1).put_object(Bucket=B, Key="a", Body=b"three")
check("PUT after DELETE reads back", s3(0).get_object(Bucket=B, Key="a")["Body"].read() == b"three")
st = [state(n, "a") for n in range(3)]
check("PUT after DELETE is live on every node", all(s[0] == "live" for s in st) and len({s[:3] for s in st}) == 1, st)

# Tagging applies to the live version on every node.
s3(2).put_object_tagging(Bucket=B, Key="a", Tagging={"TagSet": [{"Key": "pii", "Value": "yes"}]})
tags = s3(0).get_object_tagging(Bucket=B, Key="a")["TagSet"]
check("tags read back from another node", tags == [{"Key": "pii", "Value": "yes"}], tags)
st = [state(n, "a") for n in range(3)]
check("every node has the tags on the same version", all(s[3] == [["pii", "yes"]] for s in st) and len({s[1] for s in st}) == 1, st)

# An older record sent straight to a node is refused.
live = [json.loads(l)[1] for l in open(os.path.join(RUN, "node0", "cluster_location.log")) if json.loads(l)[0].endswith(":a")][-1]["Live"]
older = dict(live, seq=live["seq"] - 1, version="older-version")
r = requests.post(f"{NODES[0]}/cluster/_internal/location", json=older)
check("an older record is refused with 409", r.status_code == 409 and r.json()["current_seq"] == live["seq"], (r.status_code, r.text))
check("the refused record left the live version alone", state(0, "a")[1] == live["version"])

# An update naming a version that isn't live is refused.
stale_update = dict(live, seq=live["seq"] + 10, version="not-the-live-version", tags=[["x", "y"]])
r = requests.post(f"{NODES[0]}/cluster/_internal/location/update", json=stale_update)
check("an update for a version that isn't live is refused with 412", r.status_code == 412, (r.status_code, r.text))

# Clock skew: a node whose newest record is ahead of the coordinator's clock.
# Raise node1's record for "skew" far into the future, then DELETE through node0.
c.put_object(Bucket=B, Key="skew", Body=b"x" * 5000)
rec = [json.loads(l)[1] for l in open(os.path.join(RUN, "node1", "cluster_location.log")) if json.loads(l)[0].endswith(":skew")][-1]["Live"]
future = dict(rec, seq=rec["seq"] + 3_600_000_000_000)
r = requests.post(f"{NODES[1]}/cluster/_internal/location", json=future)
check("setup: node1 accepted a record stamped an hour ahead", r.status_code == 200, r.status_code)
s3(0).delete_object(Bucket=B, Key="skew")
st = [state(n, "skew") for n in range(3)]
check("DELETE still wins on every node when one node's record is stamped ahead", all(s[0] == "tomb" for s in st) and len(set(st)) == 1, st)

# Multipart complete tombstones the header and part records everywhere.
up = c.create_multipart_upload(Bucket=B, Key="mp")["UploadId"]
parts = []
for i in (1, 2):
    e = s3(i).upload_part(Bucket=B, Key="mp", UploadId=up, PartNumber=i, Body=bytes([i]) * (5 << 20))["ETag"]
    parts.append({"PartNumber": i, "ETag": e})
c.complete_multipart_upload(Bucket=B, Key="mp", UploadId=up, MultipartUpload={"Parts": parts})
check("multipart object reads back", len(s3(2).get_object(Bucket=B, Key="mp")["Body"].read()) == 10 << 20)
for n in range(3):
    ups = {k: v for k, v in entries(n, "").items() if up in k}
    check(f"node{n}: every record of the completed upload is a tombstone", ups and all("Tombstone" in v for v in ups.values()), list(ups.values())[:2])

# Multipart abort does the same.
up2 = c.create_multipart_upload(Bucket=B, Key="mp2")["UploadId"]
s3(1).upload_part(Bucket=B, Key="mp2", UploadId=up2, PartNumber=1, Body=b"z" * (5 << 20))
s3(2).abort_multipart_upload(Bucket=B, Key="mp2", UploadId=up2)
for n in range(3):
    ups = {k: v for k, v in entries(n, "").items() if up2 in k}
    check(f"node{n}: every record of the aborted upload is a tombstone", ups and all("Tombstone" in v for v in ups.values()), list(ups.values())[:2])

# Listing skips tombstones.
keys = sorted(o["Key"] for o in s3(1).list_objects_v2(Bucket=B).get("Contents", []))
check("listing shows only live objects", keys == ["a", "mp"], keys)

print(f"\n{len(failures)} failed" if failures else "\nall passed")
sys.exit(1 if failures else 0)
