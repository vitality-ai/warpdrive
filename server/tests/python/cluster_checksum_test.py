#!/usr/bin/env python3
# Author: Tejus Chandrashekar
"""
Checksum integration tests for the distributed/cluster S3-compatible API.

Covers the checksum-by-default behavior added to `/cluster/s3/...`:
  - every PUT gets a checksum (CRC32C) even when the client sends none
  - that checksum is echoed on GET/HEAD when the client asks for it
    (`x-amz-checksum-mode: ENABLED`)
  - on GET, the reconstructed object is re-verified against its stored
    checksum before ever being returned -- corruption on disk (bit-rot, a
    bad shard silently accepted by read-quorum) must fail the request
    instead of serving bad bytes. Checked on both the plain (erasure-coded)
    path and the content-dependent/FAC (packed) path.

Requires a running local multi-node cluster and direct filesystem access to
its node storage directories (to simulate on-disk corruption, bypassing the
API entirely). Start one with:

    (cd server && cargo build --release --bin warp_drive)
    ./scripts/launch_local_cluster.sh 5 9710 /tmp/wd-local-cluster

Then: pip3 install -r requirements.txt && python3 cluster_checksum_test.py
"""

import os
import sys
import boto3
from botocore.client import Config
from botocore.exceptions import ClientError

CLUSTER_BASE_URL = os.environ.get("CLUSTER_BASE_URL", "http://127.0.0.1:9710")
S3_ENDPOINT_URL = f"{CLUSTER_BASE_URL}/cluster/s3"
WORK_DIR = os.environ.get("CLUSTER_WORK_DIR", "/tmp/wd-local-cluster")
NODE_COUNT = int(os.environ.get("CLUSTER_NODE_COUNT", "5"))
STORAGE_DIRS = [os.path.join(WORK_DIR, f"node{i}", "storage") for i in range(NODE_COUNT)]

ACCESS_KEY = "AKIAIOSFODNN7EXAMPLE"
SECRET_KEY = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"


def create_s3_client():
    # `request_checksum_calculation='when_required'` stops newer botocore
    # versions from attaching their own client-side checksum by default --
    # without this, every PUT already carries a checksum header and the
    # server-side default-on behavior this suite tests is never exercised.
    return boto3.client(
        's3',
        endpoint_url=S3_ENDPOINT_URL,
        aws_access_key_id=ACCESS_KEY,
        aws_secret_access_key=SECRET_KEY,
        region_name='us-east-1',
        config=Config(
            signature_version='s3v4',
            s3={'addressing_style': 'path'},
            request_checksum_calculation='when_required',
        ),
    )


def set_bucket_packer(bucket: str, packer_name: str, overhead_threshold_pct: float):
    import json
    import urllib.request
    req = urllib.request.Request(
        f"{CLUSTER_BASE_URL}/cluster/_admin/bucket_config/{bucket}",
        data=json.dumps({"packer_name": packer_name, "overhead_threshold_pct": overhead_threshold_pct}).encode(),
        headers={"content-type": "application/json"},
        method="PUT",
    )
    urllib.request.urlopen(req)


def corrupt_all_occurrences_in_bucket_files(bucket_name: str, needle: bytes) -> int:
    """Flips a byte at every occurrence of `needle` in every on-disk storage
    file whose name contains `bucket_name`, across every node. Storage is a
    per-bucket append log, so more than one historical copy of a matching
    byte string can be present; flipping every occurrence (rather than just
    the first) guarantees hitting whichever copy is actually live, without
    needing to know the store's internal offset bookkeeping.
    """
    hits = 0
    for d in STORAGE_DIRS:
        if not os.path.isdir(d):
            continue
        for root, _, files in os.walk(d):
            for fn in files:
                if bucket_name not in fn:
                    continue
                path = os.path.join(root, fn)
                try:
                    with open(path, "rb") as f:
                        data = f.read()
                except OSError:
                    continue
                if needle not in data:
                    continue
                mutated = bytearray(data)
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


def test_plain_path_checksum_default_and_echo(s3):
    """A plain PUT with no client-sent checksum still gets one (CRC32C,
    the lightweight default), and it's echoed back on GET/HEAD when asked.
    """
    bucket, key = "cksum-plain-bucket", "cksum-plain-key"
    body = b"hello checksum world " * 1000

    s3.put_object(Bucket=bucket, Key=key, Body=body, ContentType="text/plain")

    got = s3.get_object(Bucket=bucket, Key=key, ChecksumMode='ENABLED')
    if got['Body'].read() != body:
        print("FAIL: GET body mismatch")
        return False
    checksum_headers = [h for h in got['ResponseMetadata']['HTTPHeaders'] if h.startswith('x-amz-checksum-')]
    if not checksum_headers:
        print("FAIL: expected a default checksum echoed on GET even though the client sent none on PUT")
        return False
    if not any('crc32c' in h for h in checksum_headers):
        print(f"FAIL: expected the lightweight CRC32C default, got {checksum_headers}")
        return False
    print(f"GET echoed default checksum: {checksum_headers}")

    head = s3.head_object(Bucket=bucket, Key=key, ChecksumMode='ENABLED')
    if not any(h.startswith('x-amz-checksum-') for h in head['ResponseMetadata']['HTTPHeaders']):
        print("FAIL: expected checksum echoed on HEAD")
        return False
    print("HEAD echoed default checksum")
    return True


def test_fac_path_checksum_default_and_echo(s3):
    """Same default-checksum + echo guarantee on the content-dependent
    (FAC/packed) path, which has its own independent code path for
    building the response (not shared with the plain path's handler).
    """
    bucket, key = "cksum-fac-bucket", "cksum-fac-key"
    set_bucket_packer(bucket, "fac", 100.0)
    body = b"A" * 100 + b"B" * 100 + b"C" * 100 + b"D" * 100

    def add_units_header(request, **kwargs):
        request.headers['x-warpd-computable-units'] = '[[0,100],[100,100],[200,100],[300,100]]'

    s3.meta.events.register('before-sign.s3.PutObject', add_units_header)
    try:
        put_resp = s3.put_object(Bucket=bucket, Key=key, Body=body, ContentType="application/octet-stream")
    finally:
        s3.meta.events.unregister('before-sign.s3.PutObject', add_units_header)

    # `x-warpd-placement: packed` on the PUT response is the authoritative
    # proof this object really went through the content-dependent path
    # rather than falling back to plain (e.g. if overhead exceeded budget).
    placement = put_resp['ResponseMetadata']['HTTPHeaders'].get('x-warpd-placement')
    if placement != 'packed':
        print(f"FAIL: expected packed placement, got {placement!r}")
        return False

    got = s3.get_object(Bucket=bucket, Key=key, ChecksumMode='ENABLED')
    if got['Body'].read() != body:
        print("FAIL: FAC GET body mismatch")
        return False
    if not any(h.startswith('x-amz-checksum-') for h in got['ResponseMetadata']['HTTPHeaders']):
        print("FAIL: expected a default checksum echoed on FAC GET")
        return False
    print("FAC path: packed, checksum default + echoed on GET")
    return True


def test_plain_path_corruption_is_detected_on_read(s3):
    """Deliberately corrupts the on-disk shard bytes for a plain object,
    bypassing the API entirely, then proves a subsequent GET is refused
    rather than silently returning corrupted bytes.
    """
    bucket, key = "cksum-plain-corrupt-bucket", "cksum-plain-corrupt-key"
    body = b"integrity check target bytes " * 500
    s3.put_object(Bucket=bucket, Key=key, Body=body, ContentType="text/plain")

    hits = corrupt_all_occurrences_in_bucket_files(bucket, b"integrity check target bytes")
    if hits == 0:
        print("FAIL: did not find on-disk shard bytes to corrupt -- test setup problem")
        return False

    try:
        bad = s3.get_object(Bucket=bucket, Key=key)
        bad_body = bad['Body'].read()
        print(f"FAIL: expected GET to be refused after corruption, got {len(bad_body)} bytes back")
        return False
    except ClientError as e:
        status = e.response['ResponseMetadata']['HTTPStatusCode']
        print(f"plain-path corruption correctly rejected: HTTP {status}")
        return status >= 500


def test_fac_path_corruption_is_detected_on_read(s3):
    """Same corruption-detection guarantee on the content-dependent (FAC)
    read path, which decodes bytes through an entirely different function
    (`packed.rs::get_object_content_dependent`) than the plain path.
    """
    bucket, key = "cksum-fac-corrupt-bucket", "cksum-fac-corrupt-key"
    set_bucket_packer(bucket, "fac", 100.0)
    body = b"W" * 100 + b"X" * 100 + b"Y" * 100 + b"Z" * 100

    def add_units_header(request, **kwargs):
        request.headers['x-warpd-computable-units'] = '[[0,100],[100,100],[200,100],[300,100]]'

    s3.meta.events.register('before-sign.s3.PutObject', add_units_header)
    try:
        put_resp = s3.put_object(Bucket=bucket, Key=key, Body=body, ContentType="application/octet-stream")
    finally:
        s3.meta.events.unregister('before-sign.s3.PutObject', add_units_header)

    if put_resp['ResponseMetadata']['HTTPHeaders'].get('x-warpd-placement') != 'packed':
        print("FAIL: test object did not actually land on the FAC path")
        return False

    hits = corrupt_all_occurrences_in_bucket_files(bucket, b"WWWWWWWWWW")
    if hits == 0:
        print("FAIL: did not find on-disk FAC shard bytes to corrupt -- test setup problem")
        return False

    try:
        bad = s3.get_object(Bucket=bucket, Key=key)
        bad_body = bad['Body'].read()
        print(f"FAIL: expected FAC GET to be refused after corruption, got {len(bad_body)} bytes back")
        return False
    except ClientError as e:
        status = e.response['ResponseMetadata']['HTTPStatusCode']
        print(f"FAC-path corruption correctly rejected: HTTP {status}")
        return status >= 500


def main():
    print("=== WarpDrive Cluster Checksum Integration Tests ===")
    print(f"Cluster base URL: {CLUSTER_BASE_URL}")
    print(f"Storage dirs: {STORAGE_DIRS}")
    print("=" * 60)

    if not any(os.path.isdir(d) for d in STORAGE_DIRS):
        print(f"No node storage directories found under {WORK_DIR} -- is the cluster running "
              f"and CLUSTER_WORK_DIR set correctly?")
        return 1

    s3 = create_s3_client()

    tests = [
        ("plain path: checksum default + echo", lambda: test_plain_path_checksum_default_and_echo(s3)),
        ("FAC path: checksum default + echo", lambda: test_fac_path_checksum_default_and_echo(s3)),
        ("plain path: corruption detected on read", lambda: test_plain_path_corruption_is_detected_on_read(s3)),
        ("FAC path: corruption detected on read", lambda: test_fac_path_corruption_is_detected_on_read(s3)),
    ]

    results = []
    for name, fn in tests:
        print(f"\n--- {name} ---")
        try:
            ok = fn()
        except Exception as e:
            print(f"Test raised an exception: {e}")
            ok = False
        results.append((name, ok))
        print(f"Result: {'PASS' if ok else 'FAIL'}")

    print("\n" + "=" * 60)
    print("SUMMARY:")
    passed = 0
    for name, ok in results:
        print(f"{name}: {'PASS' if ok else 'FAIL'}")
        if ok:
            passed += 1
    print(f"\nTotal: {passed}/{len(results)} passed")
    return 0 if passed == len(results) else 1


if __name__ == "__main__":
    sys.exit(main())
