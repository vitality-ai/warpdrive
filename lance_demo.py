"""
Builds a real Lance IVF_PQ dataset locally, uploads its files into two
WarpDrive buckets via the new /cluster/s3/ shim -- one plain, one configured
for "ivf_centroid" packing on the auxiliary.idx file's real partition
byte ranges -- then points Lance's own (real, unmodified) S3 client at
each bucket and runs real nprobe vector-search queries, comparing latency
and stripes touched. Same experiment shape as duckdb_demo.py /
pushdown_benchmark.py: same data, same real third-party reader, only the
bucket's placement configuration differs.

Usage: python3 lance_demo.py <cluster_base_url>
"""
import base64
import glob
import json
import os
import shutil
import sys
import tempfile
import time

import boto3
import lance
import numpy as np
import pyarrow as pa
from botocore.config import Config
from lance.file import LanceFileReader
from lance.indices import IndicesBuilder

from lance_real_offsets import lance_ivf_real_units, lance_data_fragment_real_units, nearest_neighbor_chain_order, verify_full_coverage
from real_workload_driver import configure_bucket_for_content_dependent_placement


def make_blob_structured_vectors(n_vectors: int, dim: int, n_topics: int, rng, blob_std: float = 1.0):
    """Real embeddings have genuine semantic clusters -- similar items end
    up measurably closer together than dissimilar ones, which is the
    entire reason IVF indexing works on them at all. Pure i.i.d. Gaussian
    noise does NOT have this property: checked directly, in 768-dim i.i.d.
    Gaussian data every pairwise distance concentrates around the same
    value (measured: 38-41, vs. the theoretical sqrt(2*dim)=39.2,
    regardless of which pair you pick) -- there is no "nearby" structure
    for any spatial packer to exploit, by construction, which is why
    IvfCentroidPacker measured no improvement on pure-noise test vectors
    no matter how the grouping algorithm was built. This generates
    `n_topics` random "topic" centers (sampled once, far apart in a
    normalized space) and draws each vector from a tight Gaussian around a
    randomly chosen topic -- real cluster structure, same shape as actual
    embeddings, not a synthetic artifact."""
    topic_centers = rng.standard_normal((n_topics, dim)).astype("float32")
    topic_centers /= np.linalg.norm(topic_centers, axis=1, keepdims=True)
    topic_centers *= np.sqrt(dim)  # same norm scale as the noise below, so blobs are separated, not swamped
    topic_ids = rng.integers(0, n_topics, size=n_vectors)
    noise = rng.standard_normal((n_vectors, dim)).astype("float32") * blob_std
    vecs = topic_centers[topic_ids] + noise
    return vecs, topic_centers, topic_ids


def build_local_dataset(n_vectors: int, dim: int, num_partitions: int, seed: int = 42, n_topics: int = 40):
    rng = np.random.default_rng(seed)
    vecs, topic_centers, topic_ids = make_blob_structured_vectors(n_vectors, dim, n_topics, rng)
    tbl = pa.table({
        "id": pa.array(range(n_vectors)),
        "vector": pa.FixedSizeListArray.from_arrays(vecs.reshape(-1), dim),
    })

    tmpdir = tempfile.mkdtemp(prefix="lance_demo_")
    uri = os.path.join(tmpdir, "data.lance")
    ds = lance.write_dataset(tbl, uri)

    builder = IndicesBuilder(ds, "vector")
    ivf_model = builder.train_ivf(num_partitions=num_partitions, sample_rate=64)
    centroids = ivf_model.centroids

    ds.create_index(
        "vector", index_type="IVF_PQ", num_partitions=num_partitions,
        num_sub_vectors=8, metric="l2", ivf_centroids=centroids, replace=True,
    )
    ds = lance.dataset(uri)
    return uri, ds, vecs, topic_centers


def find_aux_file(ds, index_name: str) -> str:
    stats = ds.index_statistics(index_name)
    return stats["indices"][0]["uri"].replace("index.idx", "auxiliary.idx")


def upload_dataset(cluster_url: str, bucket: str, local_root: str, real_units_by_rel_path: dict = None):
    """Uploads every file under local_root to {bucket}/{dataset_name}/{relative path} --
    the "data.lance/" prefix matters: Lance's own reader is pointed at
    s3://{bucket}/{dataset_name}, so it looks up keys under that exact
    prefix (e.g. "data.lance/_versions/..."), not bare "_versions/...".
    `real_units_by_rel_path` maps a relative path to its (units, ...) list
    (as returned by lance_ivf_real_units / lance_data_fragment_real_units)
    -- each such file is PUT with the x-warpd-computable-units header
    carrying those real byte ranges (+ metadata, if any); every other file
    is PUT with the header explicitly set to "false" -- the dispatch
    escape hatch -- forcing plain EC regardless of the bucket's own config.

    This matters more than it looks: a bucket configured for content-
    dependent placement but given NO header for an object doesn't fall
    back to plain by default -- it synthesizes a single whole-object unit
    and still *attempts* content-dependent packing (so that an
    object with real sub-structure but a client that forgot the header
    still gets a safe, correct answer). For a single unit under RS(3,2)
    that's a measured, real 200% overhead (comfortably under the generous
    500% threshold this demo uses for the real aux file), which means
    every OTHER file in the dataset -- the ~60MB data fragment included --
    was silently being packed as one oversized, mostly-zero-padded stripe
    instead of split evenly across k bins like the plain path does.
    Found by timing: a single data-fragment fetch took ~150ms (zero-padded
    ~61MB-per-shard) vs. plain's even ~20MB-per-shard split, and repeated
    on every result row materialized -- not a flaw in the packer's
    algorithm, a bucket-config permissiveness bug letting unrelated files
    ride along as fake content-dependent objects."""
    real_units_by_rel_path = real_units_by_rel_path or {}
    dataset_name = os.path.basename(local_root.rstrip("/"))
    s3_client = make_boto3_client(cluster_url)
    for path in glob.glob(os.path.join(local_root, "**", "*"), recursive=True):
        if os.path.isdir(path):
            continue
        rel = os.path.relpath(path, local_root)
        key = f"{dataset_name}/{rel}"
        with open(path, "rb") as f:
            data = f.read()

        units_header = "false"
        if rel in real_units_by_rel_path:
            header_units = []
            for unit_id, offset, length, meta in real_units_by_rel_path[rel]:
                entry = [offset, length, length, "opaque"]
                if meta:
                    entry.append(base64.b64encode(meta).decode())
                header_units.append(entry)
            units_header = json.dumps(header_units)

        # boto3, not a hand-signed urllib request: /cluster/s3/... enforces
        # real SigV4 (added after this script was first written against an
        # unauthenticated version of that route), and boto3 is the
        # already-proven-working client for it (server/tests/python/
        # s3_boto_test.py). x-warpd-computable-units is added as a signed
        # header via a before-sign hook, same mechanism any S3 SDK uses to
        # send a non-standard header.
        def inject_units_header(request, **kw):
            request.headers["x-warpd-computable-units"] = units_header

        s3_client.meta.events.register("before-sign.s3.PutObject", inject_units_header)
        s3_client.put_object(Bucket=bucket, Key=key, Body=data)
        s3_client.meta.events.unregister("before-sign.s3.PutObject", inject_units_header)


# /cluster/s3/... requires real SigV4 auth (added after this script was
# first written against an unauthenticated version of that route). Uses
# the project's real admin credentials (the same ones run_local_cluster.sh
# sets via WARPDRIVE_ADMIN_ACCESS_KEY/SECRET_KEY) through boto3, the
# already-proven-working client for this route.
ADMIN_ACCESS_KEY = os.environ.get("WARPDRIVE_ADMIN_ACCESS_KEY", "adminkey")
ADMIN_SECRET_KEY = os.environ.get("WARPDRIVE_ADMIN_SECRET_KEY", "adminsecretkey123456")


def make_boto3_client(cluster_url: str):
    return boto3.client(
        "s3",
        endpoint_url=f"{cluster_url}/cluster/s3",
        aws_access_key_id=ADMIN_ACCESS_KEY,
        aws_secret_access_key=ADMIN_SECRET_KEY,
        region_name="us-east-1",
        config=Config(signature_version="s3v4", s3={"addressing_style": "path"}, request_checksum_calculation="when_required"),
    )


def lance_storage_options(cluster_url: str) -> dict:
    host = cluster_url.replace("http://", "").replace("https://", "")
    return {
        "endpoint": f"http://{host}/cluster/s3",
        "allow_http": "true",
        "aws_access_key_id": ADMIN_ACCESS_KEY,
        "aws_secret_access_key": ADMIN_SECRET_KEY,
        "aws_region": "us-east-1",
        "virtual_hosted_style_request": "false",
    }


if __name__ == "__main__":
    cluster_url = sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:9710"
    n_vectors, dim, num_partitions = 20_000, 768, 283

    print(f"Building local Lance dataset: {n_vectors} vectors, dim={dim}, partitions={num_partitions}...")
    local_uri, ds, vecs, topic_centers = build_local_dataset(n_vectors, dim, num_partitions)
    aux_path = find_aux_file(ds, "vector_idx")
    aux_rel = os.path.relpath(aux_path, local_uri)
    print("aux file:", aux_rel)

    aux_units, aux_file_size = lance_ivf_real_units(local_uri, "vector_idx", aux_path)
    verify_full_coverage(aux_units, aux_file_size)
    print(f"Real partition units extracted and verified: {len(aux_units)} units, {aux_file_size} bytes, full coverage confirmed.")

    data_files = glob.glob(os.path.join(local_uri, "data", "*.lance"))
    assert len(data_files) == 1, f"expected exactly one data fragment file, found {data_files}"
    data_path = data_files[0]
    data_rel = os.path.relpath(data_path, local_uri)
    print("data fragment file:", data_rel)

    data_units, data_file_size = lance_data_fragment_real_units(data_path, batch_size=20)
    verify_full_coverage(data_units, data_file_size)
    print(f"Real row-batch units extracted and verified: {len(data_units)} units, {data_file_size} bytes, full coverage confirmed.")

    configure_bucket_for_content_dependent_placement(cluster_url, "lanceivf", overhead_threshold_pct=500.0, packer_name="ivf_centroid")
    # (plain bucket needs no config at all -- unconfigured = always plain, per the dispatch redesign)

    print("Uploading to plain bucket (lanceplain)...")
    upload_dataset(cluster_url, "lanceplain", local_uri)

    print("Uploading to ivf_centroid-configured bucket (lanceivf)...")
    upload_dataset(cluster_url, "lanceivf", local_uri, real_units_by_rel_path={aux_rel: aux_units, data_rel: data_units})

    print("\nOpening both buckets via Lance's own S3 client...")
    storage_options = lance_storage_options(cluster_url)
    remote = {}
    for bucket in ["lanceplain", "lanceivf"]:
        remote_uri = f"s3://{bucket}/data.lance"
        remote[bucket] = lance.dataset(remote_uri, storage_options=storage_options)
        count = remote[bucket].count_rows()
        print(f"  {bucket}: opened OK, count_rows={count} (expected {n_vectors})")

    rng = np.random.default_rng(7)
    n_queries = 10
    for nprobe in [4, 16, 40]:
        print(f"\n=== nprobe={nprobe} ===")
        for bucket in ["lanceplain", "lanceivf"]:
            times = []
            last_result = None
            for qi in range(n_queries):
                q = vecs[rng.integers(0, n_vectors)]
                t0 = time.perf_counter()
                res = remote[bucket].to_table(nearest={"column": "vector", "q": q, "k": 10, "nprobes": nprobe})
                times.append((time.perf_counter() - t0) * 1000)
                last_result = res
            times = times[1:]  # drop first (connection warmup)
            med = sorted(times)[len(times) // 2]
            print(f"  {bucket}: median {med:.1f}ms over {len(times)} queries (rows returned: {last_result.num_rows})")
