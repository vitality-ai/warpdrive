"""
The real benchmark, not a synthetic one: SIFT1M (small variant) from the
texmex corpus -- the same dataset LanceDB's own published benchmarks use
(GIST-1M and SIFT-1M are their standard latency/recall reporting
datasets). Real 128-dim SIFT descriptors, real queries, real ground truth
-- not blob-structured synthetic vectors. Asks the real question a vector
search benchmark is supposed to ask: not just "is it fast" but "is it
still correct" -- recall@10 against the actual published ground truth,
for both the plain and content-dependent-packed buckets.

Source: ftp.irisa.fr/local/texmex/corpus/siftsmall.tar.gz (blocked from
this network; mirrored via TileDB-Inc/TileDB-Vector-Search's GitHub
release of the identical file, byte-identical to the standard corpus).
Parsed and verified directly: groundtruth[0]'s top-10 matches an
independent brute-force exact nearest-neighbor search over the real base
vectors, confirmed before building anything on top of it.

Usage: python3 sift_benchmark.py <cluster_base_url>
"""
import os
import sys
import time

import lance
import numpy as np
import pyarrow as pa
from lance.indices import IndicesBuilder

from lance_demo import (
    find_aux_file, upload_dataset, lance_storage_options,
)
from lance_real_offsets import lance_ivf_real_units, lance_data_fragment_real_units, verify_full_coverage
from real_workload_driver import configure_bucket_for_content_dependent_placement


def read_fvecs(path: str) -> np.ndarray:
    data = np.fromfile(path, dtype="int32")
    dim = data[0]
    return data.view("float32").reshape(-1, dim + 1)[:, 1:].copy()


def read_ivecs(path: str) -> np.ndarray:
    data = np.fromfile(path, dtype="int32")
    dim = data[0]
    return data.reshape(-1, dim + 1)[:, 1:].copy()


def recall_at_k(returned_ids: list[int], ground_truth_ids: np.ndarray, k: int) -> float:
    gt_set = set(ground_truth_ids[:k].tolist())
    hit = len(set(returned_ids[:k]) & gt_set)
    return hit / k


def build_sift_dataset(base: np.ndarray, num_partitions: int, num_sub_vectors: int, sample_rate: int):
    n, dim = base.shape
    tbl = pa.table({
        "id": pa.array(range(n)),
        "vector": pa.FixedSizeListArray.from_arrays(base.reshape(-1), dim),
    })
    import tempfile
    tmpdir = tempfile.mkdtemp(prefix="sift_bench_")
    uri = os.path.join(tmpdir, "data.lance")
    ds = lance.write_dataset(tbl, uri)

    builder = IndicesBuilder(ds, "vector")
    ivf_model = builder.train_ivf(num_partitions=num_partitions, sample_rate=sample_rate)
    ds.create_index(
        "vector", index_type="IVF_PQ", num_partitions=num_partitions,
        num_sub_vectors=num_sub_vectors, metric="l2", ivf_centroids=ivf_model.centroids, replace=True,
    )
    return os.path.join(tmpdir, "data.lance"), lance.dataset(uri)


if __name__ == "__main__":
    cluster_url = sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:9710"

    print("Loading real SIFT1M (small) benchmark data...")
    base = read_fvecs("data/siftsmall_base.fvecs")
    queries = read_fvecs("data/siftsmall_query.fvecs")
    groundtruth = read_ivecs("data/siftsmall_groundtruth.ivecs")
    print(f"  base: {base.shape}, queries: {queries.shape}, groundtruth: {groundtruth.shape}")

    n, dim = base.shape
    num_partitions = 100
    print(f"Building real Lance IVF_PQ index: {n} vectors, dim={dim}, partitions={num_partitions}...")
    local_uri, ds = build_sift_dataset(base, num_partitions=num_partitions, num_sub_vectors=16, sample_rate=50)

    aux_path = find_aux_file(ds, "vector_idx")
    aux_rel = os.path.relpath(aux_path, local_uri)
    aux_units, aux_size = lance_ivf_real_units(local_uri, "vector_idx", aux_path, bins_per_stripe=3)
    verify_full_coverage(aux_units, aux_size)
    print(f"  aux units: {len(aux_units)}, {aux_size} bytes, coverage verified")

    import glob
    data_path = glob.glob(os.path.join(local_uri, "data", "*.lance"))[0]
    data_rel = os.path.relpath(data_path, local_uri)
    data_units, data_size = lance_data_fragment_real_units(data_path, batch_size=20)
    verify_full_coverage(data_units, data_size)
    print(f"  data-fragment units: {len(data_units)}, {data_size} bytes, coverage verified")

    configure_bucket_for_content_dependent_placement(cluster_url, "siftivf", overhead_threshold_pct=500.0, packer_name="ivf_centroid")

    print("Uploading to plain bucket (siftplain)...")
    upload_dataset(cluster_url, "siftplain", local_uri)
    print("Uploading to packed bucket (siftivf)...")
    upload_dataset(cluster_url, "siftivf", local_uri, real_units_by_rel_path={aux_rel: aux_units, data_rel: data_units})

    storage_options = lance_storage_options(cluster_url)
    remote = {}
    for bucket in ["siftplain", "siftivf"]:
        remote[bucket] = lance.dataset(f"s3://{bucket}/data.lance", storage_options=storage_options)
        count = remote[bucket].count_rows()
        print(f"  {bucket}: opened OK, count_rows={count} (expected {n})")

    print("\nRunning real queries from the benchmark's own query set, checking real recall@10...")
    for nprobe in [5, 10, 20]:
        print(f"\n=== nprobe={nprobe} ===")
        for bucket in ["siftplain", "siftivf"]:
            times, recalls = [], []
            for qi in range(len(queries)):
                q = queries[qi]
                t0 = time.perf_counter()
                res = remote[bucket].to_table(columns=["id"], nearest={"column": "vector", "q": q, "k": 10, "nprobes": nprobe})
                times.append((time.perf_counter() - t0) * 1000)
                recalls.append(recall_at_k(res["id"].to_pylist(), groundtruth[qi], 10))
            times_after_warmup = times[1:]
            med = sorted(times_after_warmup)[len(times_after_warmup) // 2]
            mean_recall = sum(recalls) / len(recalls)
            print(f"  {bucket}: median {med:.2f}ms over {len(times_after_warmup)} queries, mean recall@10={mean_recall:.3f} (over all {len(queries)} real queries)")
