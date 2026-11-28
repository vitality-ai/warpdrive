#!/usr/bin/env python3
"""verify.py <hashes.json> <endpoint for PUTs> <endpoint for GETs>...: PUT objects of several sizes with random bytes through
the first endpoint (64 threads), then GET every object from every endpoint and compare SHA-256. Also a multipart
upload, and save the hashes. With VERIFY_READ_ONLY=1, skip the PUTs and read back the saved hashes. Prints the counts of objects, mismatches and errors per endpoint, and exits 1 on any mismatch or error."""
import boto3, hashlib, json, os, sys
from botocore.config import Config
from concurrent.futures import ThreadPoolExecutor
saved, put_ep, get_eps = sys.argv[1], sys.argv[2], sys.argv[2:]
cfg = Config(retries={"max_attempts": 1}, max_pool_connections=64)
def client(ep):
    return boto3.client("s3", endpoint_url=ep, aws_access_key_id="AKIAIOSFODNN7EXAMPLE",
                        aws_secret_access_key="wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY", region_name="us-east-1", config=cfg)
s3 = client(put_ep); bucket = "verify"
if os.environ.get("VERIFY_READ_ONLY"):
    want = json.load(open(saved))
else:
    s3.create_bucket(Bucket=bucket)
    sizes = [0, 1, 4096, 65536, 130000, 1048576, 8388608]
    objs = [(f"o-{s}-{i}", s) for s in sizes for i in range(40 if s <= 1048576 else 8)]
    want = {}
    def put(o):
        k, s = o; b = os.urandom(s); s3.put_object(Bucket=bucket, Key=k, Body=b); return k, hashlib.sha256(b).hexdigest()
    with ThreadPoolExecutor(64) as ex:
        want.update(ex.map(put, objs))
    mp = s3.create_multipart_upload(Bucket=bucket, Key="mp")["UploadId"]; parts, h = [], hashlib.sha256()
    for n in range(1, 4):
        b = os.urandom(5 << 20 if n < 3 else 1234567); h.update(b)
        parts.append({"PartNumber": n, "ETag": s3.upload_part(Bucket=bucket, Key="mp", UploadId=mp, PartNumber=n, Body=b)["ETag"]})
    s3.complete_multipart_upload(Bucket=bucket, Key="mp", UploadId=mp, MultipartUpload={"Parts": parts}); want["mp"] = h.hexdigest()
    json.dump(want, open(saved, "w"))
bad = 0
for ep in get_eps:
    c = client(ep)
    def get(k):
        try:
            return hashlib.sha256(c.get_object(Bucket=bucket, Key=k)["Body"].read()).hexdigest() == want[k], None
        except Exception as e:
            return False, f"{k}: {e}"
    with ThreadPoolExecutor(32) as ex:
        res = list(ex.map(get, want))
    errs = [e for ok, e in res if e]; mism = sum(1 for ok, e in res if not ok and not e)
    print(f"{ep} objects={len(res)} mismatches={mism} errors={len(errs)} {errs[:2]}"); bad += mism + len(errs)
sys.exit(1 if bad else 0)
