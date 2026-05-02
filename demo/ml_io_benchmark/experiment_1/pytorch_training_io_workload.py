#!/usr/bin/env python3
"""
PyTorch training-shaped I/O workload: dataset staging + load, train loop, checkpoints, extra I/O.

Compare backends by setting STORAGE to "local", "warpdrive", or "minio".

Phases (all timed in the JSON report):
  1) Stage synthetic dataset (.pt) to storage
  2) Load dataset from storage (training-time dataset I/O)
  3) Train for EPOCHS with a small CNN; every CHECKPOINT_EVERY epochs save checkpoint to storage
  4) Checkpoint round-trip bench: SAVE_ROUNDS × (save large payload + load + verify)

Requires: pip install torch boto3

Run: cd warpdrive/demo/ml_io_benchmark/experiment_1 && python pytorch_training_io_workload.py

Report: pytorch_training_io_report_<STORAGE>_<UTC>.json (this folder)
"""

from __future__ import annotations

import io
import json
import os
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Dict, List, Optional, Tuple

import torch
import torch.nn as nn
from torch.utils.data import DataLoader, TensorDataset

# ---------------------------------------------------------------------------
# CONFIG
# ---------------------------------------------------------------------------
_ML_ROOT = Path(__file__).resolve().parent
_WARPDRIVE_ROOT = _ML_ROOT.parents[3]

# "local" | "warpdrive" | "minio"
STORAGE = "local"

EPOCHS = 12
BATCH_SIZE = 128
LR = 1e-3
NUM_WORKERS = 0  # keep 0 so dataloader I/O stays easy to reason about; data already in RAM

# Synthetic vision dataset size (float32 CHW). Total bytes ≈ N * 3 * 32 * 32 * 4.
# Warpdrive currently buffers full objects on GET: one request ≈ one large Vec in RAM.
# Low Docker memory limits → OOMKilled (exit 137). Lower N and/or CHECKPOINT_PADDING_MIB if needed.
SYNTH_NUM_SAMPLES = 65536
SYNTH_C, SYNTH_H, SYNTH_W = 3, 32, 32

# Save a training checkpoint every N epochs (in addition to end-of-training saves if aligned).
CHECKPOINT_EVERY = 3

# After training: repeated save+load of a fat checkpoint to stress PUT/GET (0 disables).
CHECKPOINT_ROUNDTRIP_ROUNDS = 3
# Extra megabytes of random tensor data embedded in each bench checkpoint (plus real state).
CHECKPOINT_PADDING_MIB = 48

S3_REGION = "us-east-1"
S3_ENDPOINT_URL = "http://localhost:9710/s3"
S3_BUCKET = "default"
AUTH_FILE = _WARPDRIVE_ROOT / "demo" / "test_user_auth.txt"

MINIO_ENDPOINT_URL = os.environ.get("MINIO_ENDPOINT_URL", "http://127.0.0.1:9000")
MINIO_ACCESS_KEY = os.environ.get("MINIO_ACCESS_KEY", "minioadmin")
MINIO_SECRET_KEY = os.environ.get("MINIO_SECRET_KEY", "minioadmin")
MINIO_BUCKET = os.environ.get("MINIO_BUCKET", "ml-io-bench")

LOCAL_ARTIFACT_DIR = _ML_ROOT / "data" / "pt_training_io"

torch.manual_seed(42)
# ---------------------------------------------------------------------------


def _fix_read_auth(path: Path) -> Dict[str, str]:
    """Same as other ml/ scripts: key=value lines."""
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

    creds = _fix_read_auth(AUTH_FILE)
    ak, sk = creds.get("access_key"), creds.get("secret_key")
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


def s3_bucket(storage: str) -> str:
    if storage == "minio":
        return MINIO_BUCKET
    if storage == "warpdrive":
        return S3_BUCKET
    raise ValueError(storage)


def build_s3_client(storage: str):
    if storage == "warpdrive":
        return build_warpdrive_s3_client()
    if storage == "minio":
        return build_minio_s3_client()
    raise ValueError(storage)


def ensure_s3_bucket(client: Any, bucket: str) -> None:
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


def s3_put_bytes(client: Any, bucket: str, key: str, body: bytes) -> None:
    client.put_object(Bucket=bucket, Key=key, Body=body)


def s3_get_bytes(client: Any, bucket: str, key: str) -> bytes:
    return client.get_object(Bucket=bucket, Key=key)["Body"].read()


def _torch_save_bytes(obj: Any) -> bytes:
    buf = io.BytesIO()
    torch.save(obj, buf)
    return buf.getvalue()


def _torch_load_bytes(b: bytes, map_location: torch.device) -> Any:
    bio = io.BytesIO(b)
    try:
        return torch.load(bio, map_location=map_location, weights_only=False)
    except TypeError:
        bio.seek(0)
        return torch.load(bio, map_location=map_location)


def _torch_load_path(path: Path, map_location: str) -> Any:
    try:
        return torch.load(path, map_location=map_location, weights_only=False)
    except TypeError:
        return torch.load(path, map_location=map_location)


class SmallCNN32(nn.Module):
    def __init__(self) -> None:
        super().__init__()
        self.features = nn.Sequential(
            nn.Conv2d(3, 32, 3, padding=1),
            nn.ReLU(inplace=True),
            nn.MaxPool2d(2),
            nn.Conv2d(32, 64, 3, padding=1),
            nn.ReLU(inplace=True),
            nn.MaxPool2d(2),
        )
        self.classifier = nn.Sequential(
            nn.Flatten(),
            nn.Linear(64 * 8 * 8, 128),
            nn.ReLU(inplace=True),
            nn.Linear(128, 10),
        )

    def forward(self, x: torch.Tensor) -> torch.Tensor:
        return self.classifier(self.features(x))


def build_synthetic_dataset(
    n: int, c: int, h: int, w: int, device: torch.device
) -> Tuple[torch.Tensor, torch.Tensor]:
    images = torch.randn(n, c, h, w, dtype=torch.float32, device=device)
    labels = torch.randint(0, 10, (n,), device=device)
    return images.cpu(), labels.cpu()


def main() -> None:
    storage = STORAGE
    if storage not in ("local", "warpdrive", "minio"):
        raise SystemExit('STORAGE must be "local", "warpdrive", or "minio"')

    wall0 = time.perf_counter()
    run_id = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    report_path = _ML_ROOT / f"pytorch_training_io_report_{storage}_{run_id}.json"

    device = torch.device("cuda" if torch.cuda.is_available() else "cpu")
    bucket = s3_bucket(storage) if storage != "local" else ""
    prefix = f"pt-training-io/{run_id}"
    data_key = f"{prefix}/dataset.pt"

    report: Dict[str, Any] = {
        "created_at_utc": datetime.now(timezone.utc).isoformat(),
        "device": str(device),
        "config": {
            "run_id": run_id,
            "storage": storage,
            "epochs": EPOCHS,
            "batch_size": BATCH_SIZE,
            "synth_num_samples": SYNTH_NUM_SAMPLES,
            "synth_shape_chw": [SYNTH_C, SYNTH_H, SYNTH_W],
            "checkpoint_every": CHECKPOINT_EVERY,
            "checkpoint_roundtrip_rounds": CHECKPOINT_ROUNDTRIP_ROUNDS,
            "checkpoint_padding_mib": CHECKPOINT_PADDING_MIB,
            "s3_endpoint_url": (
                MINIO_ENDPOINT_URL
                if storage == "minio"
                else (S3_ENDPOINT_URL if storage == "warpdrive" else None)
            ),
            "s3_bucket": bucket or None,
            "artifact_prefix": prefix,
            "local_artifact_dir": str(LOCAL_ARTIFACT_DIR),
            "report_path": str(report_path),
        },
        "timings_sec": {},
        "dataset_bytes": 0,
        "epochs": [],
        "checkpoint_saves": [],
        "checkpoint_roundtrip": [],
    }

    LOCAL_ARTIFACT_DIR.mkdir(parents=True, exist_ok=True)
    local_dataset_path = LOCAL_ARTIFACT_DIR / f"{run_id}_dataset.pt"

    s3_client: Optional[Any] = None
    if storage != "local":
        s3_client = build_s3_client(storage)
        if storage == "minio":
            ensure_s3_bucket(s3_client, bucket)

    # --- Build synthetic dataset tensor dict in memory ---
    t0 = time.perf_counter()
    images, labels = build_synthetic_dataset(
        SYNTH_NUM_SAMPLES, SYNTH_C, SYNTH_H, SYNTH_W, device
    )
    dataset_obj = {"images": images, "labels": labels}
    build_s = time.perf_counter() - t0

    # --- Stage dataset to storage ---
    t0 = time.perf_counter()
    dataset_bytes = 0
    if storage == "local":
        torch.save(dataset_obj, local_dataset_path)
        dataset_bytes = local_dataset_path.stat().st_size
    else:
        assert s3_client is not None
        body = _torch_save_bytes(dataset_obj)
        dataset_bytes = len(body)
        s3_put_bytes(s3_client, bucket, data_key, body)
    stage_dataset_s = time.perf_counter() - t0
    report["dataset_bytes"] = dataset_bytes
    report["timings_sec"]["synth_dataset_build_s"] = round(build_s, 4)
    report["timings_sec"]["dataset_stage_to_storage_s"] = round(stage_dataset_s, 4)

    # --- Load dataset from storage (training I/O) ---
    t0 = time.perf_counter()
    if storage == "local":
        loaded = _torch_load_path(local_dataset_path, map_location="cpu")
    else:
        assert s3_client is not None
        raw = s3_get_bytes(s3_client, bucket, data_key)
        loaded = _torch_load_bytes(raw, map_location=torch.device("cpu"))
    load_dataset_s = time.perf_counter() - t0
    report["timings_sec"]["dataset_load_from_storage_s"] = round(load_dataset_s, 4)

    train_images = loaded["images"]
    train_labels = loaded["labels"]
    t_dl = time.perf_counter()
    ds = TensorDataset(train_images, train_labels)
    loader = DataLoader(
        ds,
        batch_size=BATCH_SIZE,
        shuffle=True,
        num_workers=NUM_WORKERS,
        pin_memory=device.type == "cuda",
    )
    report["timings_sec"]["dataloader_init_s"] = round(time.perf_counter() - t_dl, 4)

    model = SmallCNN32().to(device)
    opt = torch.optim.Adam(model.parameters(), lr=LR)
    crit = nn.CrossEntropyLoss()

    checkpoint_index = 0

    def save_ckpt(tag: str, epoch: int, extra_pad: Optional[torch.Tensor] = None) -> Dict[str, Any]:
        nonlocal checkpoint_index
        payload: Dict[str, Any] = {
            "epoch": epoch,
            "tag": tag,
            "state_dict": {k: v.cpu() for k, v in model.state_dict().items()},
            "optimizer": opt.state_dict(),
        }
        if extra_pad is not None:
            payload["_io_pad"] = extra_pad
        t_s = time.perf_counter()
        nbytes = 0
        if storage == "local":
            p = LOCAL_ARTIFACT_DIR / f"{run_id}_ckpt_{checkpoint_index:04d}.pt"
            torch.save(payload, p)
            nbytes = p.stat().st_size
        else:
            assert s3_client is not None
            b = _torch_save_bytes(payload)
            nbytes = len(b)
            key = f"{prefix}/ckpt_{checkpoint_index:04d}_{tag}.pt"
            s3_put_bytes(s3_client, bucket, key, b)
        elapsed = time.perf_counter() - t_s
        checkpoint_index += 1
        return {
            "tag": tag,
            "epoch": epoch,
            "save_s": round(elapsed, 4),
            "bytes": nbytes,
        }

    epoch_rows: List[Dict[str, Any]] = []
    ckpt_saves: List[Dict[str, Any]] = []

    for epoch in range(1, EPOCHS + 1):
        model.train()
        t_tr = time.perf_counter()
        running = 0.0
        n_batches = 0
        for x, y in loader:
            x, y = x.to(device), y.to(device)
            opt.zero_grad(set_to_none=True)
            logits = model(x)
            loss = crit(logits, y)
            loss.backward()
            opt.step()
            running += loss.item()
            n_batches += 1
        train_s = time.perf_counter() - t_tr

        epoch_rows.append(
            {
                "epoch": epoch,
                "train_s": round(train_s, 4),
                "train_loss": round(running / max(1, n_batches), 4),
                "batches": n_batches,
            }
        )

        if epoch % CHECKPOINT_EVERY == 0 or epoch == EPOCHS:
            ckpt_saves.append(save_ckpt(f"epoch_{epoch}", epoch))

    report["epochs"] = epoch_rows
    report["checkpoint_saves"] = ckpt_saves
    report["timings_sec"]["sum_train_epochs_s"] = round(
        sum(r["train_s"] for r in epoch_rows), 4
    )
    report["timings_sec"]["sum_checkpoint_save_s"] = round(
        sum(c["save_s"] for c in ckpt_saves), 4
    )

    # --- Fat checkpoint round-trip (I/O stress) ---
    roundtrip_rows: List[Dict[str, Any]] = []
    if CHECKPOINT_ROUNDTRIP_ROUNDS > 0:
        pad_elems = max(1, (CHECKPOINT_PADDING_MIB * 1024 * 1024) // 4)
        padding_tensor = torch.randn(pad_elems, dtype=torch.float32)
    else:
        padding_tensor = torch.empty(0)

    for r in range(CHECKPOINT_ROUNDTRIP_ROUNDS):
        t0 = time.perf_counter()
        if storage == "local":
            p = LOCAL_ARTIFACT_DIR / f"{run_id}_bench_ckpt_{r}.pt"
            payload = {
                "round": r,
                "state_dict": {k: v.cpu() for k, v in model.state_dict().items()},
                "_io_pad": padding_tensor.clone(),
            }
            torch.save(payload, p)
            save_s = time.perf_counter() - t0
            t1 = time.perf_counter()
            loaded_p = _torch_load_path(p, map_location="cpu")
            load_s = time.perf_counter() - t1
            nbytes = p.stat().st_size
        else:
            assert s3_client is not None
            payload = {
                "round": r,
                "state_dict": {k: v.cpu() for k, v in model.state_dict().items()},
                "_io_pad": padding_tensor.clone(),
            }
            body = _torch_save_bytes(payload)
            nbytes = len(body)
            key = f"{prefix}/bench_ckpt_{r}.pt"
            s3_put_bytes(s3_client, bucket, key, body)
            save_s = time.perf_counter() - t0
            t1 = time.perf_counter()
            raw = s3_get_bytes(s3_client, bucket, key)
            loaded_p = _torch_load_bytes(raw, map_location=torch.device("cpu"))
            load_s = time.perf_counter() - t1
        ok = loaded_p["round"] == r and loaded_p["_io_pad"].shape == padding_tensor.shape
        roundtrip_rows.append(
            {
                "round": r,
                "save_s": round(save_s, 4),
                "load_s": round(load_s, 4),
                "bytes": nbytes,
                "verify_ok": bool(ok),
            }
        )

    report["checkpoint_roundtrip"] = roundtrip_rows
    report["timings_sec"]["sum_roundtrip_save_s"] = round(
        sum(x["save_s"] for x in roundtrip_rows), 4
    )
    report["timings_sec"]["sum_roundtrip_load_s"] = round(
        sum(x["load_s"] for x in roundtrip_rows), 4
    )
    report["timings_sec"]["total_wall_s"] = round(time.perf_counter() - wall0, 4)

    # Summary rates (MiB/s) for big phases
    def _mib_s(seconds: float, nbytes: int) -> float:
        if seconds <= 0:
            return 0.0
        return round((nbytes / (1024 * 1024)) / seconds, 3)

    report["summary"] = {
        "dataset_load_mib_per_s": _mib_s(load_dataset_s, dataset_bytes),
        "mean_training_checkpoint_save_mib_per_s": (
            round(
                sum(_mib_s(c["save_s"], c["bytes"]) for c in ckpt_saves) / len(ckpt_saves),
                3,
            )
            if ckpt_saves
            else 0.0
        ),
        "mean_roundtrip_save_mib_per_s": (
            round(
                sum(_mib_s(x["save_s"], x["bytes"]) for x in roundtrip_rows)
                / len(roundtrip_rows),
                3,
            )
            if roundtrip_rows
            else 0.0
        ),
        "mean_roundtrip_load_mib_per_s": (
            round(
                sum(_mib_s(x["load_s"], x["bytes"]) for x in roundtrip_rows)
                / len(roundtrip_rows),
                3,
            )
            if roundtrip_rows
            else 0.0
        ),
    }

    report_path.write_text(json.dumps(report, indent=2), encoding="utf-8")
    print(json.dumps(report["timings_sec"], indent=2))
    print(json.dumps(report["summary"], indent=2))
    print(f"saved_report={report_path}")


if __name__ == "__main__":
    main()
