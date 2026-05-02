#!/usr/bin/env python3
"""
I/O-bound data workload: stage medium/large shards, then repeatedly fetch them.

Designed so wall time is dominated by reading data (S3 GetObject or local disk), not compute.
A tiny PyTorch op runs per shard so the loop still looks like a training pipeline.

Requires: pip install torch boto3

Run from repo (venv activated):
  cd warpdrive/demo/ml_io_benchmark/experiment_1 && python io_bound_data_workload.py

Edit STORAGE and CONFIG below. Writes io_bound_run_report_<storage>_<UTC>.json in this folder per run.

S3-compatible modes: "warpdrive" (Vitality auth file) and "minio" (env or defaults; see MINIO_* below).
"""

from __future__ import annotations

import hashlib
import json
import os
import threading
import time
from concurrent.futures import ThreadPoolExecutor, as_completed
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Callable, Dict, List, Optional, Tuple

import torch

# ---------------------------------------------------------------------------
# CONFIG
# ---------------------------------------------------------------------------
_ML_ROOT = Path(__file__).resolve().parent
_WARPDRIVE_ROOT = _ML_ROOT.parents[3]

# "warpdrive" | "minio" | "local"
STORAGE = "minio"

# Synthetic shards (total staged ≈ NUM_SHARDS * SHARD_SIZE_BYTES).
# Larger shards → fewer HTTP/S3 round trips → often better MiB/s on Warpdrive (latency amortized).
# Smaller total? e.g. NUM_SHARDS=4, SHARD_SIZE_BYTES=64*1024*1024
NUM_SHARDS = 8
SHARD_SIZE_BYTES = 128 * 1024 * 1024  # 128 MiB each → 1 GiB staged; 3 GiB read across FETCH_ROUNDS
FETCH_ROUNDS = 3  # full pass over all shards per round

# Parallel fetches per round (1 = sequential). Each thread uses its own S3 client.
# With large shards, too much parallelism can stress the server; 2–4 is often safer than 8.
FETCH_WORKERS = 4

# Warpdrive / S3
S3_ENDPOINT_URL = "http://localhost:9710/s3"
S3_BUCKET = "default"
S3_PREFIX = "ml-io-stress/shards"
S3_REGION = "us-east-1"
AUTH_FILE = _WARPDRIVE_ROOT / "demo" / "test_user_auth.txt"

# MinIO (S3-compatible). Defaults match `docker run minio/minio server /data`.
# Override with env: MINIO_ENDPOINT_URL, MINIO_ACCESS_KEY, MINIO_SECRET_KEY, MINIO_BUCKET.
MINIO_ENDPOINT_URL = os.environ.get("MINIO_ENDPOINT_URL", "http://127.0.0.1:9000")
MINIO_ACCESS_KEY = os.environ.get("MINIO_ACCESS_KEY", "minioadmin")
MINIO_SECRET_KEY = os.environ.get("MINIO_SECRET_KEY", "minioadmin")
MINIO_BUCKET = os.environ.get("MINIO_BUCKET", "ml-io-bench")

# Local storage mirror (when STORAGE == "local")
LOCAL_SHARD_DIR = _ML_ROOT / "data" / "io_stress_shards"

# If True, skip staging (expect objects/files to already exist)
SKIP_STAGE = False

# Minimal compute per shard: first N bytes → float tensor → one small matmul
COMPUTE_SLICE_BYTES = 8192
COMPUTE_OUT_DIM = 64
# Report path is set in main() so each run gets a unique filename.
# ---------------------------------------------------------------------------

_thread_local = threading.local()
_tiny_weight: Optional[torch.Tensor] = None


def read_auth_file(path: Path) -> Dict[str, str]:
    if not path.exists():
        return {}
    creds: Dict[str, str] = {}
    for line in path.read_text().splitlines():
        line = line.strip()
        if "=" in line and not line.startswith("#"):
            k, v = line.split("=", 1)
            creds[k.strip()] = v.strip()
    return creds


def _s3_boto_config():
    from botocore.config import Config as BotoConfig

    return BotoConfig(
        signature_version="s3v4",
        s3={"addressing_style": "path"},
        read_timeout=900,
        connect_timeout=60,
        retries={"max_attempts": 5, "mode": "adaptive"},
    )


def build_warpdrive_s3_client():
    import boto3

    creds = read_auth_file(AUTH_FILE)
    ak = creds.get("access_key")
    sk = creds.get("secret_key")
    if not ak or not sk:
        raise SystemExit(f"Need access_key and secret_key in {AUTH_FILE}")
    return boto3.client(
        "s3",
        endpoint_url=S3_ENDPOINT_URL,
        region_name=S3_REGION,
        aws_access_key_id=ak,
        aws_secret_access_key=sk,
        config=_s3_boto_config(),
    )


def build_minio_s3_client():
    import boto3

    return boto3.client(
        "s3",
        endpoint_url=MINIO_ENDPOINT_URL,
        region_name=S3_REGION,
        aws_access_key_id=MINIO_ACCESS_KEY,
        aws_secret_access_key=MINIO_SECRET_KEY,
        config=_s3_boto_config(),
    )


def s3_bucket_name() -> str:
    if STORAGE == "minio":
        return MINIO_BUCKET
    return S3_BUCKET


def build_s3_client_for_storage():
    if STORAGE == "warpdrive":
        return build_warpdrive_s3_client()
    if STORAGE == "minio":
        return build_minio_s3_client()
    raise RuntimeError("build_s3_client_for_storage: STORAGE must be warpdrive or minio")


def get_thread_s3_client():
    if not hasattr(_thread_local, "client"):
        _thread_local.client = build_s3_client_for_storage()
    return _thread_local.client


def ensure_s3_bucket(client, bucket: str) -> None:
    """Create bucket if missing (used for MinIO dev; avoids relying on pre-created buckets)."""
    from botocore.exceptions import ClientError

    try:
        client.head_bucket(Bucket=bucket)
        return
    except ClientError as e:
        code = e.response.get("Error", {}).get("Code", "")
        status = e.response.get("ResponseMetadata", {}).get("HTTPStatusCode")
        if code in ("404", "NoSuchBucket") or status == 404:
            try:
                client.create_bucket(Bucket=bucket)
            except ClientError as e2:
                c2 = e2.response.get("Error", {}).get("Code", "")
                if c2 not in ("BucketAlreadyOwnedByYou", "BucketAlreadyExists"):
                    raise
            return
        raise


def shard_keys() -> List[str]:
    return [f"{S3_PREFIX.rstrip('/')}/shard_{i:04d}.bin" for i in range(NUM_SHARDS)]


def stage_warpdrive() -> Dict[str, Any]:
    t0 = time.perf_counter()
    client = build_warpdrive_s3_client()
    bucket = s3_bucket_name()
    keys = shard_keys()
    total_up = 0
    chunk = os.urandom(min(SHARD_SIZE_BYTES, 65536))
    payload = (chunk * (SHARD_SIZE_BYTES // len(chunk) + 1))[:SHARD_SIZE_BYTES]
    for key in keys:
        client.put_object(Bucket=bucket, Key=key, Body=payload)
        total_up += len(payload)
    elapsed = time.perf_counter() - t0
    return {"keys": keys, "bytes_uploaded": total_up, "stage_s": round(elapsed, 4)}


def stage_minio() -> Dict[str, Any]:
    t0 = time.perf_counter()
    client = build_minio_s3_client()
    ensure_s3_bucket(client, MINIO_BUCKET)
    keys = shard_keys()
    total_up = 0
    chunk = os.urandom(min(SHARD_SIZE_BYTES, 65536))
    payload = (chunk * (SHARD_SIZE_BYTES // len(chunk) + 1))[:SHARD_SIZE_BYTES]
    for key in keys:
        client.put_object(Bucket=MINIO_BUCKET, Key=key, Body=payload)
        total_up += len(payload)
    elapsed = time.perf_counter() - t0
    return {"keys": keys, "bytes_uploaded": total_up, "stage_s": round(elapsed, 4)}


def stage_local() -> Dict[str, Any]:
    LOCAL_SHARD_DIR.mkdir(parents=True, exist_ok=True)
    t0 = time.perf_counter()
    keys = shard_keys()
    paths: List[Path] = []
    chunk = os.urandom(min(SHARD_SIZE_BYTES, 65536))
    payload = (chunk * (SHARD_SIZE_BYTES // len(chunk) + 1))[:SHARD_SIZE_BYTES]
    total = 0
    for key in keys:
        name = Path(key).name
        p = LOCAL_SHARD_DIR / name
        p.write_bytes(payload)
        paths.append(p)
        total += len(payload)
    elapsed = time.perf_counter() - t0
    # Use strings so report JSON is serializable (Path is not).
    return {
        "paths": [str(p.resolve()) for p in paths],
        "bytes_written": total,
        "stage_s": round(elapsed, 4),
    }


def tiny_compute_on_bytes(body: bytes) -> None:
    global _tiny_weight
    n = min(COMPUTE_SLICE_BYTES, len(body))
    if n == 0:
        return
    x = torch.frombuffer(bytearray(body[:n]), dtype=torch.uint8).float().unsqueeze(0)
    if _tiny_weight is None or _tiny_weight.shape[1] != x.shape[1]:
        _tiny_weight = torch.randn(x.shape[1], COMPUTE_OUT_DIM)
    _ = x @ _tiny_weight


def fetch_one_s3(key: str) -> Tuple[str, int, str]:
    client = get_thread_s3_client()
    bucket = s3_bucket_name()
    t0 = time.perf_counter()
    resp = client.get_object(Bucket=bucket, Key=key)
    body = resp["Body"].read()
    fetch_s = time.perf_counter() - t0
    digest = hashlib.sha256(body).hexdigest()
    t1 = time.perf_counter()
    tiny_compute_on_bytes(body)
    compute_s = time.perf_counter() - t1
    return key, len(body), f"{fetch_s:.6f}:{compute_s:.6f}:{digest[:16]}"


def fetch_one_local(path: Path) -> Tuple[str, int, str]:
    t0 = time.perf_counter()
    body = path.read_bytes()
    fetch_s = time.perf_counter() - t0
    digest = hashlib.sha256(body).hexdigest()
    t1 = time.perf_counter()
    tiny_compute_on_bytes(body)
    compute_s = time.perf_counter() - t1
    return str(path), len(body), f"{fetch_s:.6f}:{compute_s:.6f}:{digest[:16]}"


def run_round_parallel(
    items: List[Any],
    fetch_fn: Callable[[Any], Tuple[str, int, str]],
) -> Dict[str, Any]:
    t0 = time.perf_counter()
    bytes_read = 0
    fetch_times: List[float] = []
    compute_times: List[float] = []
    if FETCH_WORKERS <= 1:
        for it in items:
            _, nbytes, meta = fetch_fn(it)
            bytes_read += nbytes
            fs, cs, _ = meta.split(":", 2)
            fetch_times.append(float(fs))
            compute_times.append(float(cs))
    else:
        with ThreadPoolExecutor(max_workers=FETCH_WORKERS) as ex:
            futs = {ex.submit(fetch_fn, it): it for it in items}
            for fut in as_completed(futs):
                _, nbytes, meta = fut.result()
                bytes_read += nbytes
                fs, cs, _ = meta.split(":", 2)
                fetch_times.append(float(fs))
                compute_times.append(float(cs))
    wall = time.perf_counter() - t0
    n = max(1, len(fetch_times))
    return {
        "round_wall_s": round(wall, 4),
        "bytes_read": bytes_read,
        "throughput_mib_per_s": round(
            (bytes_read / (1024 * 1024)) / max(wall, 1e-9),
            3,
        ),
        "avg_fetch_ms": round(1000.0 * sum(fetch_times) / n, 3),
        "max_fetch_ms": round(1000.0 * max(fetch_times), 3),
        "sum_compute_ms": round(1000.0 * sum(compute_times), 3),
    }


def main() -> None:
    wall0 = time.perf_counter()
    run_id = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    report_path = _ML_ROOT / f"io_bound_run_report_{STORAGE}_{run_id}.json"
    report: Dict[str, Any] = {
        "created_at_utc": datetime.now(timezone.utc).isoformat(),
        "config": {
            "run_id": run_id,
            "local_run_report_path": str(report_path),
            "storage": STORAGE,
            "num_shards": NUM_SHARDS,
            "shard_size_bytes": SHARD_SIZE_BYTES,
            "fetch_rounds": FETCH_ROUNDS,
            "fetch_workers": FETCH_WORKERS,
            "s3_prefix": S3_PREFIX,
            "s3_bucket": s3_bucket_name() if STORAGE in ("warpdrive", "minio") else None,
            "s3_endpoint_url": (
                MINIO_ENDPOINT_URL
                if STORAGE == "minio"
                else (S3_ENDPOINT_URL if STORAGE == "warpdrive" else None)
            ),
            "local_shard_dir": str(LOCAL_SHARD_DIR),
            "skip_stage": SKIP_STAGE,
        },
        "staging": {},
        "rounds": [],
        "timings_sec": {},
    }

    keys = shard_keys()
    local_paths = [LOCAL_SHARD_DIR / Path(k).name for k in keys]

    if not SKIP_STAGE:
        if STORAGE == "warpdrive":
            report["staging"] = stage_warpdrive()
        elif STORAGE == "minio":
            report["staging"] = stage_minio()
        elif STORAGE == "local":
            report["staging"] = stage_local()
        else:
            raise SystemExit('STORAGE must be "warpdrive", "minio", or "local"')
    else:
        report["staging"] = {"skipped": True}

    if STORAGE in ("warpdrive", "minio"):
        fetch_fn = fetch_one_s3
        items: List[Any] = keys
    else:
        fetch_fn = fetch_one_local
        items = local_paths
        for p in local_paths:
            if not p.exists():
                raise SystemExit(f"SKIP_STAGE but missing file: {p}")

    total_bytes = NUM_SHARDS * SHARD_SIZE_BYTES * FETCH_ROUNDS
    rounds_out: List[Dict[str, Any]] = []
    for r in range(FETCH_ROUNDS):
        rounds_out.append({"round_index": r + 1, **run_round_parallel(items, fetch_fn)})
    report["rounds"] = rounds_out

    sum_round_wall = sum(x["round_wall_s"] for x in rounds_out)
    sum_compute_ms = sum(x["sum_compute_ms"] for x in rounds_out)
    stage_s = report["staging"].get("stage_s", 0.0) if isinstance(report["staging"], dict) else 0.0

    report["timings_sec"] = {
        "stage_s": stage_s,
        "sum_round_wall_s": round(sum_round_wall, 4),
        "sum_compute_all_rounds_ms": round(sum_compute_ms, 3),
        "total_wall_s": round(time.perf_counter() - wall0, 4),
        "total_bytes_moved_read": total_bytes,
        "effective_read_mib_per_s": round(
            (total_bytes / (1024 * 1024)) / max(sum_round_wall, 1e-9),
            3,
        ),
    }
    report["analysis"] = {
        "per_round_throughput_mib_per_s": [x["throughput_mib_per_s"] for x in rounds_out],
        "compute_ms_vs_read_wall_ms": round(
            1000.0 * sum_round_wall,
            1,
        ),
        "sum_compute_all_rounds_ms": sum_compute_ms,
        "note": "Workload is I/O bound if sum_compute_all_rounds_ms is tiny vs 1000*sum_round_wall_s. "
        "With FETCH_WORKERS>1, round_wall_s is wall time; avg_fetch_ms is mean per-shard latency.",
    }

    t0 = time.perf_counter()
    report_path.write_text(json.dumps(report, indent=2), encoding="utf-8")
    report["timings_sec"]["write_report_local_s"] = round(time.perf_counter() - t0, 4)
    report_path.write_text(json.dumps(report, indent=2), encoding="utf-8")

    print(json.dumps(report["timings_sec"], indent=2))
    print(f"saved_report={report_path}")
    print(
        f"compute_ms (all rounds) vs read wall ms: {sum_compute_ms:.1f} vs {1000.0 * sum_round_wall:.1f}"
    )


if __name__ == "__main__":
    main()
