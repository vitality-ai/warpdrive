"""
Real file-offset computable units for a Lance IVF_PQ vector index's
auxiliary.idx file -- the vector-search counterpart to
parquet_real_offsets.py. Builds a WarpDrive bucket's computable-unit header
so each IVF partition becomes its own unit, tagged with a real spatial-rank
derived from the index's own real centroids (not synthetic), for
IvfCentroidPacker to use.

What's real here, checked directly against a built index, not assumed:
- Partition vector counts and centroids come from `ds.index_statistics()`.
- The `__pq_code` column's on-disk buffer position/size come from
  `LanceFileReader(auxiliary.idx).metadata()` -- real physical byte
  layout, not inferred from the format spec alone.
- The buffer is confirmed flat and fixed-width (8 bytes/row for
  num_sub_vectors=8, num_bits=8): sum(partition_counts) * 8 + 80 ==
  buffer.size, exactly, for every index this has been checked against.

What's a declared assumption, not verified against Lance's own encoding
spec: the 80 leftover bytes are a small fixed header *before* the flat
per-partition data, not padding at the end or interleaved. Only the
arithmetic is checked (it's self-consistent); this is not a claim that the
exact byte boundary matches Lance's internal encoding documentation, which
does not publicly specify this buffer's micro-layout. If this assumption
is wrong, partition boundaries would be off by a constant, which would
still produce *a* valid non-overlapping, full-coverage partition --
just not exactly aligned to real partition boundaries. Flagged honestly,
not silently assumed correct.

The `_rowid` column uses a different, non-flat encoding (observed: 8400
bytes for 5000 rows, not the 40000 a flat 8-bytes/row layout would need)
and is NOT sliced per-partition here -- it's folded into the leading
framing unit instead, same treatment as Parquet's leading magic bytes.
Rowid data is a small fraction of this file's bytes (8400 of ~48000, here)
and isn't what the IVF selective-read story is about; the PQ-code column
is the dominant cost and the thing actually being made selectively
fetchable.
"""
import struct

import lance
from lance.file import LanceFileReader


def nearest_neighbor_chain_order(centroids) -> list[int]:
    """Greedy nearest-neighbor ordering over real centroids: start at
    partition 0, repeatedly jump to the nearest not-yet-visited centroid.
    O(n^2), fine for a few hundred to low thousands of partitions.

    SUPERSEDED -- kept only for reference.
    A 1-D chain only guarantees *consecutive* points in the chain are
    close; it does NOT guarantee an arbitrary query's k-nearest-centroids
    set (a "ball" in high-dimensional space) maps to a contiguous interval
    of the chain. Measured directly: real IVF queries against a
    chain-ordered pack touched anywhere from 1 to 55 of 95 stripes, and
    were consistently *slower* than no packing at all (580-620ms plain vs
    1650-1820ms packed, RS(3,2), nprobe 4/16/40). See
    kmeans_cluster_assignments below, which clusters directly in centroid space
    instead of linearizing it."""
    import numpy as np

    c = np.asarray(centroids, dtype="float64")
    n = c.shape[0]
    visited = [False] * n
    order = [0]
    visited[0] = True
    for _ in range(n - 1):
        last = c[order[-1]]
        dists = np.linalg.norm(c - last, axis=1)
        dists[visited] = np.inf
        nxt = int(np.argmin(dists))
        order.append(nxt)
        visited[nxt] = True
    return order


def kmeans_cluster_assignments(centroids, bins_per_stripe: int, seed: int = 0, max_iters: int = 50) -> list[int]:
    """Clusters real IVF centroids into ceil(n/bins_per_stripe) groups via
    a plain numpy Lloyd's k-means (no sklearn dependency needed at this
    scale -- a few hundred points, low dozens of clusters, converges in a
    handful of iterations). Unlike a 1-D chain (see
    nearest_neighbor_chain_order above), k-means directly optimizes
    multi-dimensional proximity, so an arbitrary query's nprobe-nearest
    centroids -- which tend to be mutually close in the same real sense
    k-means clusters on -- are far more likely to fall inside one or two
    clusters instead of being scattered arbitrarily.

    Returns a list where result[i] = the real cluster id of partition i
    (0-indexed) -- IvfCentroidPacker groups units by *exact* cluster id
    equality (not sorted-order proximity: an earlier version returned a
    flattened rank and chunked every `k`, which only guarantees chain-
    adjacent ranks are close, not that a whole cluster's units land
    together when a cluster has more than `k` members). Within each
    cluster id, the Rust side runs real FAC-style bin-packing, not a
    one-unit-per-bin rule, so a cluster bigger than `bins_per_stripe`
    still packs efficiently instead of spilling into one stripe per
    `bins_per_stripe` partitions regardless of actual cluster size."""
    import numpy as np

    c = np.asarray(centroids, dtype="float64")
    n = c.shape[0]
    n_clusters = max(1, (n + bins_per_stripe - 1) // bins_per_stripe)

    rng = np.random.default_rng(seed)
    centers = c[rng.choice(n, size=n_clusters, replace=False)].copy()
    assignments = np.zeros(n, dtype=int)

    for _ in range(max_iters):
        dists = np.linalg.norm(c[:, None, :] - centers[None, :, :], axis=2)
        new_assignments = dists.argmin(axis=1)
        if np.array_equal(new_assignments, assignments) and _ > 0:
            break
        assignments = new_assignments
        for k in range(n_clusters):
            members = c[assignments == k]
            if len(members) > 0:
                centers[k] = members.mean(axis=0)

    return assignments.tolist()


def lance_ivf_real_units(
    dataset_uri: str, index_name: str, aux_file_path: str, bins_per_stripe: int = 3
) -> tuple[list[tuple[str, int, int, bytes]], int]:
    """Returns (units, file_size) where each unit is
    (unit_id, offset, len, metadata_bytes) -- metadata_bytes is the
    partition's little-endian u32 spatial rank, matching
    IvfCentroidPacker's expected format. Includes two framing units
    (leading: everything before the pq_code buffer, trailing: everything
    after) so every byte of the file is covered, same requirement as
    Parquet's framing units.

    `bins_per_stripe` must match the bucket's actual erasure-coding `k`
    (RS(k,m)'s data-shard count) -- it sizes the k-means clustering so
    each cluster's partitions land in (ideally) one stripe's worth of
    bins. Wrong value doesn't break correctness, just blunts the point:
    IvfCentroidPacker still groups every `k` consecutive ranks into one
    stripe regardless, so a mismatched `bins_per_stripe` just means
    cluster boundaries and stripe boundaries don't line up as neatly."""
    ds = lance.dataset(dataset_uri)
    stats = ds.index_statistics(index_name)
    index_info = stats["indices"][0]
    counts = [p["size"] for p in index_info["partitions"]]
    centroids = index_info["centroids"]  # real, same order as `counts`

    cluster_id_of_partition = kmeans_cluster_assignments(centroids, bins_per_stripe=bins_per_stripe)

    reader = LanceFileReader(aux_file_path)
    md = reader.metadata()
    pq_buffer = md.columns[1].pages[0].buffers[1]  # see module docstring

    header_bytes = pq_buffer.size - sum(counts) * 8
    assert header_bytes >= 0, f"flat-layout assumption violated: buffer {pq_buffer.size} < {sum(counts)*8}"

    units = []
    cursor = pq_buffer.position + header_bytes
    for i, count in enumerate(counts):
        length = count * 8
        if length > 0:
            cluster_bytes = struct.pack("<I", cluster_id_of_partition[i])
            units.append((f"partition{i}", cursor, length, cluster_bytes))
        cursor += length

    with open(aux_file_path, "rb") as f:
        f.seek(0, 2)
        file_size = f.tell()

    pq_data_start = pq_buffer.position + header_bytes
    pq_data_end = pq_buffer.position + pq_buffer.size
    leading = [("_leading", 0, pq_data_start, b"")] if pq_data_start > 0 else []
    trailing = [("_trailing", pq_data_end, file_size - pq_data_end, b"")] if pq_data_end < file_size else []

    all_units = leading + units + trailing
    return all_units, file_size


def lance_data_fragment_real_units(data_file_path: str, batch_size: int = 20) -> tuple[list[tuple[str, int, int, bytes]], int]:
    """Real per-row-batch computable units for a Lance *data fragment* file
    (the one holding the actual `id`/`vector` columns `take` reads from to
    materialize result rows) -- this, not the auxiliary.idx partition
    layout, is where real query latency was found to live: reconstructing
    a ~60MB plain file to fetch 10 scattered result rows, 2-3 times per
    query, measured at ~150ms each. Unlike IVF partition access (which has
    real spatial structure an nprobe query can exploit), which specific
    rows a `take` needs are essentially uniform-random among all rows --
    there's no "nearby rows get queried together" relationship to encode
    as metadata. So these units carry NO cluster-id metadata at all, which
    makes IvfCentroidPacker's own fallback (units with no metadata all
    land in one group, cluster id u32::MAX, then get real `fac_pack`
    applied within it) degenerate into exactly FacPacker's own size-based
    bin-packing -- the right tool for "many small same-size units, no
    locality to exploit", reusing the bucket's existing fixed packer
    policy rather than inventing a third one.

    Checked directly, not assumed: the `vector` column (a fixed-size-list
    float column) is a single flat buffer, size exactly
    `num_rows * dim * 4` -- zero internal header, zero padding, confirmed
    by exact arithmetic match, not approximation. Its *position* within
    the file is NOT assumed to be byte 0 -- found directly, column write
    order on disk doesn't necessarily match schema declaration order (a
    768-dim/20k-row dataset put it first; a 128-dim/10k-row one did not),
    so whatever comes before and after the vector buffer is folded into
    leading/trailing framing units, same treatment the aux file's magic
    bytes and footer got. The `id` column uses a different, non-flat
    encoding in both cases (sequential integers compress well) and is
    never sliced per-row -- it's just part of whichever framing region it
    falls into."""
    reader = LanceFileReader(data_file_path)
    md = reader.metadata()
    num_rows = md.num_rows

    vector_idx = md.schema.get_field_index("vector")
    assert vector_idx >= 0, f"no 'vector' field in schema: {md.schema.names}"
    vector_buffer = md.columns[vector_idx].pages[0].buffers[-1]
    row_stride = vector_buffer.size // num_rows
    assert row_stride * num_rows == vector_buffer.size, "vector column is not flat/fixed-width as assumed"

    with open(data_file_path, "rb") as f:
        f.seek(0, 2)
        file_size = f.tell()

    units = []
    leading = [("_leading", 0, vector_buffer.position, b"")] if vector_buffer.position > 0 else []
    for i in range(0, num_rows, batch_size):
        rows_in_batch = min(batch_size, num_rows - i)
        offset = vector_buffer.position + i * row_stride
        length = rows_in_batch * row_stride
        units.append((f"rowbatch{i // batch_size}", offset, length, b""))

    vector_end = vector_buffer.position + vector_buffer.size
    trailing = [("_trailing", vector_end, file_size - vector_end, b"")] if vector_end < file_size else []

    return leading + units + trailing, file_size


def verify_full_coverage(units: list[tuple[str, int, int, bytes]], total_len: int) -> None:
    by_offset = sorted(units, key=lambda u: u[1])
    pos = 0
    for unit_id, offset, length, _meta in by_offset:
        if offset != pos:
            raise AssertionError(f"gap or overlap before unit {unit_id}: expected offset {pos}, got {offset}")
        pos += length
    if pos != total_len:
        raise AssertionError(f"units cover {pos} bytes, file is {total_len} bytes")
