# Positioning vs S3 Tables, and the lakehouse benchmark plan

Status: design/strategy. Decides how the user-defined-placement work relates to
AWS S3 Tables (managed Iceberg), and sets up the experiment that validates the
bet against a market-relevant baseline.

## The decision: engine, not table service

AWS S3 Tables is managed Iceberg (dedicated table buckets) + automatic
maintenance (compaction, snapshots, unreferenced-file cleanup) + an Iceberg
REST catalog. Its performance story (up to 3×) is almost entirely **sort/z-order
compaction** — periodically *rewriting files* so queries skip more of them.

S3 Tables and WarpDrive's placement thesis attack the same problem (skip data
via layout) at different granularity and time:

| | S3 Tables | WarpDrive placement |
|---|---|---|
| Granularity | file-level (reorganize across files) | sub-object stripe-level (within a Parquet file) |
| When | periodic rewrite (compaction, real I/O) | at write time, no rewrite |
| Needs | catalog + maintenance engine | the placement engine already built |

**We do not become a table service.** That is a lakehouse (catalog, compaction,
snapshot isolation, schema/partition evolution) competing head-on with AWS,
Databricks, Snowflake, and it abandons the moat (programmable sub-object
placement). Instead:

1. **Parquet/Iceberg-layout-awareness becomes the flagship built-in grouping
   policy.** The table's own structure (row groups, column chunks, stats, sort
   spec) *is* the grouping signal, so WarpDrive delivers intra-object pushdown
   transparently, **with no compaction rewrite**. The format supplies the
   affinity; we own stripe formation + placement.
2. **The Iceberg REST catalog is the interface.** Spark/Trino/DuckDB/Athena
   point at WarpDrive unmodified; the engine underneath places instead of
   compacts.

Thesis: *the storage engine that makes Iceberg/Parquet fast without a compaction
pass, drop-in through the Iceberg REST catalog.*

## The honest caveat

Complementary, not substitutes. S3 Tables' z-order reorganizes *across files*;
WarpDrive placement skips *within an object* (row groups/columns in one Parquet
file — Fusion's 149× result). WarpDrive at write time cannot globally re-sort
across objects the way compaction can. Defensible claim: WarpDrive makes
intra-object pushdown free, immediate, and reassembly-free on erasure-coded
data, and can sit *underneath* an Iceberg table while a catalog/compaction layer
handles cross-file clustering.

## The decisive experiment (A2, made market-relevant)

Placement-based intra-object skipping vs an S3-Tables-style z-order/compaction
baseline, same Iceberg/Parquet workload, measuring **both** query bytes-read /
latency **and** write+maintenance cost (WarpDrive pays zero compaction; S3
Tables pays rewrite I/O). If WarpDrive matches the query skipping with no
compaction pass, that is the pitch, measured.

Build order:
1. **Parquet parser** — read a real Parquet file's footer to extract column
   chunk byte ranges + stats per row group. These become WarpDrive computable
   units (`[offset, len, uncompressed_len, codec, metadata]`) with a locality
   key (e.g. column id) so a grouping policy can co-locate a column's chunks.
2. **Grouping policy** — co-locate units by locality key (one stripe per
   column, or per sort-key range), vs FacPacker (size-only) as baseline.
3. **Measure** — for a column-selective read, stripes-touched / bytes-read,
   `bykey` vs FacPacker vs plain; plus write cost (no compaction) vs a z-order
   compaction baseline.

## Upstream S3 consumers to drive experiments (from the literature)

Experiments should replay how real S3 consumers read/write, not synthetic
shapes. Each row: object model, access pattern, the hypothesis it tests.

| Class | Object & units | Access pattern | Tests |
|---|---|---|---|
| Lakehouse (Parquet/Iceberg/Delta) | Parquet file; row groups + column chunks + footer | footer-at-end read; row-group pruning (min/max); column projection | A2: co-locate footer + hot columns → fewer stripes/bytes read |
| Streaming (WarpStream/Kafka-on-S3) | segment; record batches | batched appends; tail reads; random historical on rewind | A1: write amplification / stripe-count under append-heavy load |
| Vector (Lance/Turbopuffer) | index; IVF partitions | co-probed partitions read together | A2-vector: reproduce Fusion's 41× Lance, safely |
| LSM/KV (SlateDB/TidesDB) | SST; blocks | batch writes, cached reads, compaction rewrites | A6: GC/compaction holes by policy |
| Serverless OLTP (Neon) | page image / WAL | random page reads, sequential WAL | write-amp + random-read locality |

References: Qd-tree (SIGMOD'20, workload-learned layout, layout-orthogonal-to-
grouping); CRUSH (pools/PGs vs placement; why the system owns grouping→physical
units); skipping-oriented partitioning (VLDB'17); PushdownDB / FlexPushdownDB
(pushdown value); AWS S3 Tables sort/z-order compaction.
