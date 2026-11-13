"""
Runs the same three real workloads (TPC-H Q6/Q1 via DuckDB, SIFT1M-small
via Lance IVF_PQ, 20k-vector row-batch via Lance) against a real MinIO
cluster instead of WarpDrive -- same datasets, same unmodified query
tools, just pointed at a different S3-compatible endpoint. This is the
"MinIO" bar in the paper's comparison: a real, independently maintained,
erasure-coded object store, not a stand-in or a simulation.

MinIO is always plain/whole-object storage here -- it has no
content-dependent placement concept, so there is only ever one number
per experiment from this script, matching the "default" column's role
in Table I, not the "workload-aware" one.

Usage: python3 minio_bench.py <endpoint_host:port> [access_key] [secret_key]
Example: python3 minio_bench.py 10.128.0.17:9000
"""
import glob
import json
import os
import sys
import time

import boto3
import duckdb
import lance
import numpy as np

from sift_benchmark import build_sift_dataset, read_fvecs, read_ivecs, recall_at_k
from lance_demo import build_local_dataset


def minio_client(endpoint: str, access_key: str, secret_key: str):
    return boto3.client(
        "s3",
        endpoint_url=f"http://{endpoint}",
        aws_access_key_id=access_key,
        aws_secret_access_key=secret_key,
        region_name="us-east-1",
    )


def ensure_bucket(client, bucket: str) -> None:
    try:
        client.head_bucket(Bucket=bucket)
    except Exception:
        client.create_bucket(Bucket=bucket)
    # Fairness: WarpDrive's /cluster/ API has no auth check at all, so the
    # benchmarked *reads* here should carry no SigV4 signing/verification
    # overhead either. Setup calls (this function, upload_file) stay on the
    # authenticated admin client -- they aren't part of any timed query --
    # but we open the bucket to anonymous GetObject/ListBucket so the actual
    # query clients (DuckDB's s3 extension, Lance's storage_options below)
    # can read it unsigned.
    policy = {
        "Version": "2012-10-17",
        "Statement": [{
            "Effect": "Allow",
            "Principal": "*",
            "Action": ["s3:GetObject", "s3:ListBucket"],
            "Resource": [f"arn:aws:s3:::{bucket}", f"arn:aws:s3:::{bucket}/*"],
        }],
    }
    client.put_bucket_policy(Bucket=bucket, Policy=json.dumps(policy))


def upload_dir(client, local_root: str, bucket: str, prefix: str) -> int:
    n = 0
    for root, _dirs, files in os.walk(local_root):
        for f in files:
            full = os.path.join(root, f)
            rel = os.path.relpath(full, local_root)
            key = f"{prefix}/{rel}"
            client.upload_file(full, bucket, key)
            n += 1
    return n


def minio_lance_storage_options(endpoint: str, access_key: str, secret_key: str) -> dict:
    # Fairness: unsigned/anonymous reads against the public-read bucket
    # policy set in ensure_bucket, instead of SigV4 -- matches WarpDrive's
    # cluster API, which has no auth check on its read path either.
    # access_key/secret_key are unused here, kept only so callers don't
    # need two different function signatures.
    return {
        "endpoint": f"http://{endpoint}",
        "allow_http": "true",
        "aws_region": "us-east-1",
        "virtual_hosted_style_request": "false",
        "skip_signature": "true",
        "allow_anonymous": "true",
    }


def run_tpch(client, endpoint: str, access_key: str, secret_key: str) -> dict:
    bucket = "tpchminio"
    ensure_bucket(client, bucket)
    parquet_path = "data/lineitem_sf1.0_sorted.parquet"
    print(f"  uploading {os.path.getsize(parquet_path)/1e6:.1f} MB lineitem.parquet to MinIO...")
    client.upload_file(parquet_path, bucket, "lineitem.parquet")

    con = duckdb.connect()
    con.execute("INSTALL httpfs; LOAD httpfs; INSTALL tpch; LOAD tpch;")
    # Fairness: no s3_access_key_id/secret_access_key set, so DuckDB's httpfs
    # sends unsigned requests -- matches WarpDrive's cluster API, which has
    # no auth check on its read path either. Relies on the public-read
    # bucket policy set in ensure_bucket.
    con.execute(f"SET s3_endpoint='{endpoint}'; SET s3_use_ssl=false; SET s3_url_style='path';")
    queries = dict(con.execute("SELECT query_nr, query FROM tpch_queries()").fetchall())

    results = {}
    for qnum, reps in [(6, 6), (1, 6)]:
        sql = queries[qnum].replace("FROM\n    lineitem", f"FROM\n    read_parquet('s3://{bucket}/lineitem.parquet')")
        times = []
        last_result = None
        for _ in range(reps):
            t0 = time.perf_counter()
            last_result = con.execute(sql).fetchall()
            times.append((time.perf_counter() - t0) * 1000)
        times = times[1:]
        med = sorted(times)[len(times) // 2]
        results[f"Q{qnum}"] = med
        print(f"  MinIO Q{qnum}: median {med:.1f}ms over {len(times)} runs, result rows={len(last_result)}")
    return results


def run_sift_small(client, endpoint: str, access_key: str, secret_key: str) -> dict:
    bucket = "siftminio"
    ensure_bucket(client, bucket)
    base = read_fvecs("data/siftsmall_base.fvecs")
    queries = read_fvecs("data/siftsmall_query.fvecs")
    groundtruth = read_ivecs("data/siftsmall_groundtruth.ivecs")
    n, dim = base.shape
    print(f"  building real Lance IVF_PQ index: {n} vectors, dim={dim}...")
    local_uri, ds = build_sift_dataset(base, num_partitions=100, num_sub_vectors=16, sample_rate=50)
    nfiles = upload_dir(client, local_uri, bucket, "data.lance")
    print(f"  uploaded {nfiles} files to MinIO")

    storage_options = minio_lance_storage_options(endpoint, access_key, secret_key)
    remote = lance.dataset(f"s3://{bucket}/data.lance", storage_options=storage_options)
    count = remote.count_rows()
    assert count == n, f"expected {n} rows, got {count}"

    out = {}
    for nprobe in [5, 10, 20]:
        times, recalls = [], []
        for qi in range(len(queries)):
            q = queries[qi]
            t0 = time.perf_counter()
            res = remote.to_table(columns=["id"], nearest={"column": "vector", "q": q, "k": 10, "nprobes": nprobe})
            times.append((time.perf_counter() - t0) * 1000)
            recalls.append(recall_at_k(res["id"].to_pylist(), groundtruth[qi], 10))
        times_after = times[1:]
        med = sorted(times_after)[len(times_after) // 2]
        mean_recall = sum(recalls) / len(recalls)
        out[nprobe] = {"median_ms": med, "recall@10": mean_recall}
        print(f"  MinIO SIFT nprobe={nprobe}: median {med:.2f}ms over {len(times_after)} queries, recall@10={mean_recall:.3f}")
    return out


def run_row_batch(client, endpoint: str, access_key: str, secret_key: str) -> dict:
    bucket = "lanceminio"
    ensure_bucket(client, bucket)
    n_vectors, dim, num_partitions = 20_000, 768, 283
    print(f"  building real Lance dataset: {n_vectors} vectors, dim={dim}...")
    local_uri, ds, vecs, _topic_centers = build_local_dataset(n_vectors, dim, num_partitions)
    nfiles = upload_dir(client, local_uri, bucket, "data.lance")
    print(f"  uploaded {nfiles} files to MinIO")

    storage_options = minio_lance_storage_options(endpoint, access_key, secret_key)
    remote = lance.dataset(f"s3://{bucket}/data.lance", storage_options=storage_options)
    count = remote.count_rows()
    assert count == n_vectors, f"expected {n_vectors} rows, got {count}"

    rng = np.random.default_rng(7)
    n_queries = 10
    out = {}
    for nprobe in [4, 16, 40]:
        times = []
        for _qi in range(n_queries):
            q = vecs[rng.integers(0, n_vectors)]
            t0 = time.perf_counter()
            remote.to_table(nearest={"column": "vector", "q": q, "k": 10, "nprobes": nprobe})
            times.append((time.perf_counter() - t0) * 1000)
        times = times[1:]
        med = sorted(times)[len(times) // 2]
        out[nprobe] = med
        print(f"  MinIO row-batch nprobe={nprobe}: median {med:.1f}ms over {len(times)} queries")
    return out


if __name__ == "__main__":
    if len(sys.argv) < 2:
        print(__doc__)
        sys.exit(1)
    endpoint = sys.argv[1]
    access_key = sys.argv[2] if len(sys.argv) > 2 else "minioadmin"
    secret_key = sys.argv[3] if len(sys.argv) > 3 else "minioadmin123"

    client = minio_client(endpoint, access_key, secret_key)

    all_results = {}
    print("=== TPC-H (MinIO) ===")
    all_results["tpch"] = run_tpch(client, endpoint, access_key, secret_key)
    print("\n=== SIFT1M-small (MinIO) ===")
    all_results["sift"] = run_sift_small(client, endpoint, access_key, secret_key)
    print("\n=== Row-batch (MinIO) ===")
    all_results["rowbatch"] = run_row_batch(client, endpoint, access_key, secret_key)

    print("\n=== JSON summary ===")
    print(json.dumps(all_results, indent=2))
