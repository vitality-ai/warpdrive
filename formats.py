"""
Two format-specific computable-unit extractors, feeding the same
generic fac_core.construct_stripes(). Each returns a flat list of
fac_core.Unit plus a `groups` mapping unit_id -> "object id" (the
granularity knob decides how finely/coarsely units are grouped before
this list is built).
"""

import io
import numpy as np
import pyarrow as pa
import pyarrow.parquet as pq

from fac_core import Unit

# ---------------------------------------------------------------------------
# Parquet
# ---------------------------------------------------------------------------

def make_synthetic_tpch_lineitem_like(n_rows: int, n_row_groups: int, seed: int) -> bytes:
    """A TPC-H-lineitem-shaped table (16 columns, configurable row groups),
    matching the table Fusion itself evaluates on (their real file: 16
    columns, 10 row groups, 10GB). Mixed cardinality columns so per-column
    compression ratio varies realistically (Fusion reports median 9.3x,
    max 63.5x compression ratio across lineitem's columns)."""
    rng = np.random.default_rng(seed)
    cols = {
        "orderkey": rng.integers(1, n_rows // 4, n_rows, dtype=np.int64),
        "partkey": rng.integers(1, 200_000, n_rows, dtype=np.int64),
        "suppkey": rng.integers(1, 10_000, n_rows, dtype=np.int32),
        "linenumber": rng.integers(1, 8, n_rows, dtype=np.int8),
        "quantity": rng.integers(1, 50, n_rows, dtype=np.int8),          # low cardinality -> high compression
        "extendedprice": rng.uniform(900, 105000, n_rows).round(2),
        "discount": rng.choice(np.arange(0, 0.11, 0.01).round(2), n_rows),  # low cardinality
        "tax": rng.choice(np.arange(0, 0.09, 0.01).round(2), n_rows),       # low cardinality
        "returnflag": rng.choice(np.array(["A", "N", "R"]), n_rows),        # 3 distinct
        "linestatus": rng.choice(np.array(["O", "F"]), n_rows),             # 2 distinct
        "shipdate": rng.integers(8000, 10000, n_rows, dtype=np.int32),
        "commitdate": rng.integers(8000, 10000, n_rows, dtype=np.int32),
        "receiptdate": rng.integers(8000, 10000, n_rows, dtype=np.int32),
        "shipinstruct": rng.choice(np.array(["DELIVER IN PERSON", "COLLECT COD", "NONE", "TAKE BACK RETURN"]), n_rows),
        "shipmode": rng.choice(np.array(["AIR", "RAIL", "SHIP", "TRUCK", "MAIL", "FOB", "REG AIR"]), n_rows),
        "comment": rng.choice(np.array([f"comment-{i}" for i in range(50_000)]), n_rows),  # high cardinality -> low compression
    }
    table = pa.table(cols)
    buf = io.BytesIO()
    rows_per_group = n_rows // n_row_groups
    pq.write_table(table, buf, row_group_size=rows_per_group, compression="snappy",
                    use_dictionary=True)
    return buf.getvalue()


def parquet_units(parquet_bytes: bytes, granularity: str) -> list[Unit]:
    """granularity: 'column_chunk' (Fusion's own choice, finest -- never
    split a column chunk), 'row_group' (merge all column chunks in a row
    group into one unit), 'whole_file' (single unit)."""
    pf = pq.ParquetFile(io.BytesIO(parquet_bytes))
    md = pf.metadata

    if granularity == "whole_file":
        total = sum(
            md.row_group(rg).column(c).total_compressed_size
            for rg in range(md.num_row_groups) for c in range(md.num_columns)
        )
        return [Unit("file", total)]

    units = []
    for rg in range(md.num_row_groups):
        if granularity == "row_group":
            size = sum(md.row_group(rg).column(c).total_compressed_size for c in range(md.num_columns))
            units.append(Unit(f"rg{rg}", size))
        elif granularity == "column_chunk":
            for c in range(md.num_columns):
                cc = md.row_group(rg).column(c)
                units.append(Unit(f"rg{rg}_c{c}", cc.total_compressed_size))
        else:
            raise ValueError(granularity)
    return units


def parquet_finest_to_storage_map(n_row_groups: int, n_columns: int, granularity: str) -> dict[str, str]:
    """Maps the TRUE finest-grained id (always column_chunk, 'rg{r}_c{c}')
    to whatever storage-unit id it ends up inside at this granularity.
    Purely structural (row/column counts are fixed by the generator
    regardless of random seed), so no parquet bytes needed here. Query
    demand is always defined at finest granularity so different storage
    granularities are compared on the *same* logical query, not a query
    redefined by however many storage units happen to exist."""
    mapping = {}
    for rg in range(n_row_groups):
        for c in range(n_columns):
            fine_id = f"rg{rg}_c{c}"
            if granularity == "column_chunk":
                mapping[fine_id] = fine_id
            elif granularity == "row_group":
                mapping[fine_id] = f"rg{rg}"
            elif granularity == "whole_file":
                mapping[fine_id] = "file"
            else:
                raise ValueError(granularity)
    return mapping


def parquet_units_batched(parquet_bytes: bytes, batch_size: int) -> list[Unit]:
    """Same batching mechanism as vector_partition_units: flatten the finest
    (column_chunk) units in a fixed order (row-group-major), then merge
    every `batch_size` consecutive ones into one storage unit. batch_size=1
    reproduces 'column_chunk', batch_size=n_columns reproduces 'row_group',
    batch_size=n_row_groups*n_columns reproduces 'whole_file' -- but any
    value in between is now a real, distinct sweep point, not just three
    hand-picked labels."""
    pf = pq.ParquetFile(io.BytesIO(parquet_bytes))
    md = pf.metadata
    flat = [
        md.row_group(rg).column(c).total_compressed_size
        for rg in range(md.num_row_groups) for c in range(md.num_columns)
    ]
    units = []
    for i in range(0, len(flat), batch_size):
        chunk = flat[i:i + batch_size]
        units.append(Unit(f"batch{i // batch_size}", sum(chunk)))
    return units


def parquet_finest_to_storage_map_batched(n_row_groups: int, n_columns: int, batch_size: int) -> dict[str, str]:
    """Maps each true finest id 'rg{r}_c{c}' (in the same row-group-major
    order parquet_units_batched flattens over) to the batch id it lands in."""
    mapping = {}
    idx = 0
    for rg in range(n_row_groups):
        for c in range(n_columns):
            mapping[f"rg{rg}_c{c}"] = f"batch{idx // batch_size}"
            idx += 1
    return mapping


def parquet_total_bytes(parquet_bytes: bytes) -> int:
    md = pq.ParquetFile(io.BytesIO(parquet_bytes)).metadata
    return sum(
        md.row_group(rg).column(c).total_compressed_size
        for rg in range(md.num_row_groups) for c in range(md.num_columns)
    )


# ---------------------------------------------------------------------------
# IVF vector-index partitions (ANN search pushdown)
# ---------------------------------------------------------------------------

# The computable unit for IVF search is one partition (posting list): a
# nearest-neighbor query only needs the partitions closest to its query
# vector, so splitting one across a stripe forces reassembly before distance
# computation can even run -- Fusion's exact fragmentation argument, applied
# to ANN search instead of column-chunk pushdown, unlike a Neon layer (which
# is fetched by the pageserver for reconstruction, not evaluated by a query,
# so it does not carry this same pushdown reasoning).
#
# nlist (partition count) is grounded in FAISS's own indexing guidance
# (facebookresearch/faiss wiki, "Guidelines to choose an index"): nlist should
# be 4*sqrt(N) to 16*sqrt(N) for a dataset of N vectors; we use the low end.
# Embedding dimension (768, float32) matches common embedding-model output
# width (e.g., BERT/BGE-base).
EMBED_DIM = 768


def vector_nlist(n_vectors: int) -> int:
    return int(round(4 * (n_vectors ** 0.5)))


def vector_partition_units(n_vectors: int, embed_dim: int, nlist: int, granularity: int, seed: int) -> list[Unit]:
    """granularity: batch size -- how many consecutive partitions get merged
    into one computable unit (1 = finest, matching "per-partition"; N>1 =
    coarser grouping, same batching mechanism as parquet_units_batched).
    Partition sizes come from a Dirichlet-multinomial split of n_vectors
    across nlist partitions: real IVF clusters are unbalanced, not uniform."""
    rng = np.random.default_rng(seed)
    probs = rng.dirichlet(np.ones(nlist) * 5.0)
    counts = rng.multinomial(n_vectors, probs)
    sizes = counts.astype(np.int64) * embed_dim * 4  # float32 vectors

    if granularity <= 1:
        return [Unit(f"partition{i}", int(sizes[i])) for i in range(nlist)]

    units = []
    for i in range(0, nlist, granularity):
        chunk = sizes[i:i + granularity]
        units.append(Unit(f"batch{i // granularity}", int(chunk.sum())))
    return units


def vector_finest_partitions(n_vectors: int, embed_dim: int, nlist: int, seed: int) -> list[Unit]:
    """The TRUE finest granularity for the vector index is always one whole
    partition -- used as the fixed logical query-demand basis regardless of
    storage granularity (see parquet_finest_to_storage_map's docstring for
    why)."""
    return vector_partition_units(n_vectors, embed_dim, nlist, granularity=1, seed=seed)


def vector_finest_to_storage_map(nlist: int, granularity: int) -> dict[str, str]:
    if granularity <= 1:
        return {f"partition{i}": f"partition{i}" for i in range(nlist)}
    mapping = {}
    for i in range(nlist):
        mapping[f"partition{i}"] = f"batch{i // granularity}"
    return mapping
