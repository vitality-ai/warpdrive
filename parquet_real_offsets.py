"""
Real file-offset computable units for a Parquet file -- unlike formats.py's
parquet_units/parquet_units_batched, which only track column-chunk *sizes*
for the simulator's abstract bin-packing model (fac_core.Unit has no offset
field; real_workload_driver.py's synthetic_body() fills in size-matched
filler bytes, not real file content, since that's all the storage-overhead
measurement ever needed).

This module exists for a different purpose: storing the *actual* bytes of
a real Parquet file in WarpDrive, with computable-unit boundaries that are
real byte ranges within that file -- required for DuckDB (or anything else
speaking real Parquet) to read it back and get a valid file, footer and all.

Checked directly against pyarrow's own metadata (not assumed): column
chunks written by pq.write_table tile the file with zero gaps between them,
in exactly row-group-major, column-minor order -- the same flat order
formats.py's own parquet_units_batched already iterates in. Only two gaps
exist in the whole file: the 4-byte leading "PAR1" magic, and the trailing
footer (FileMetaData + 4-byte length + "PAR1" magic) after the last column
chunk. Both are covered here as their own framing units, so every byte of
the file is covered by exactly one unit -- required for exact
reconstruction, since WarpDrive's content-dependent GET zero-fills any byte
range not covered by a unit.
"""
import io

import pyarrow.parquet as pq


def column_chunk_byte_range(col_meta) -> tuple[int, int]:
    """The real [start, end) byte range of one column chunk's data in the
    file -- dictionary page first if present, else straight to the data
    page, spanning total_compressed_size bytes. This is the standard way
    to locate a column chunk's bytes (file_offset itself is unreliable,
    not what we want here)."""
    start = col_meta.dictionary_page_offset if col_meta.dictionary_page_offset else col_meta.data_page_offset
    return start, start + col_meta.total_compressed_size


def parquet_real_units(parquet_bytes: bytes, batch_size: int = 1) -> list[tuple[str, int, int]]:
    """Returns (unit_id, offset, len) triples covering the ENTIRE file with
    no gaps: a leading magic-header unit, the column chunks (merged every
    `batch_size` consecutive ones in row-group-major order, batch_size=1
    reproduces column_chunk granularity), and a trailing footer unit.
    Framing units use codec 'opaque' (not pushdown-capable, doesn't matter
    which bin they land in); column-chunk units are also 'opaque' here --
    real Parquet encoding, not this project's zlib/f64 stand-in, so
    pushdown on these specifically needs a real Parquet-page ColumnCodec
    (not yet built, see docs/Distributed-Engine-Plan.md's phase 4 "not yet
    done" list) before these units could be pushdown-filtered too. Byte
    coverage and DuckDB-readability don't depend on that.
    """
    pf = pq.ParquetFile(io.BytesIO(parquet_bytes))
    md = pf.metadata

    # (start, end) pairs, not (start, length) -- column_chunk_byte_range's
    # own return type.
    flat_ranges = [
        column_chunk_byte_range(md.row_group(rg).column(c))
        for rg in range(md.num_row_groups)
        for c in range(md.num_columns)
    ]

    units: list[tuple[str, int, int]] = [("_leading_magic", 0, flat_ranges[0][0])]
    for i in range(0, len(flat_ranges), batch_size):
        chunk = flat_ranges[i : i + batch_size]
        start = chunk[0][0]
        end = chunk[-1][1]
        units.append((f"batch{i // batch_size}", start, end - start))

    last_end = flat_ranges[-1][1]
    units.append(("_trailing_footer", last_end, len(parquet_bytes) - last_end))

    return units


def verify_full_coverage(units: list[tuple[str, int, int]], total_len: int) -> None:
    """Asserts the units tile [0, total_len) with no gaps and no overlaps --
    the property that makes exact reconstruction possible at all. Cheap,
    and worth checking every time rather than trusting it silently."""
    by_offset = sorted(units, key=lambda u: u[1])
    pos = 0
    for unit_id, offset, length in by_offset:
        if offset != pos:
            raise AssertionError(f"gap or overlap before unit {unit_id}: expected offset {pos}, got {offset}")
        pos += length
    if pos != total_len:
        raise AssertionError(f"units cover {pos} bytes, file is {total_len} bytes")
