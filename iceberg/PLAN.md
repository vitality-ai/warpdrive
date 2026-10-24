# Iceberg compatibility interface for WarpDrive

Goal: let standard Iceberg clients and query engines (pyiceberg, Spark, Trino,
DuckDB) use WarpDrive as the warehouse, and eventually as the REST catalog, so
WarpDrive's content-dependent placement powers Iceberg tables with no changes to
those engines.

Layers (build bottom-up; evaluate each before the next):
1. **Warehouse / FileIO** — WarpDrive's S3 API holds the Iceberg objects
   (metadata.json, manifests, data files). EVALUATE FIRST: can a real Iceberg
   client (pyiceberg) create a table, append data, read it back, time-travel,
   with WarpDrive S3 as the object store? Find the S3-compat gaps here before
   anything else — a REST catalog can't paper over a broken FileIO.
2. **REST catalog** — serve the Iceberg REST Catalog protocol (/v1/config,
   namespaces, tables, commits) so external engines connect to WarpDrive
   directly, metadata pointer stored in WarpDrive.
3. **Placement bridge** — the differentiator: parse the table's Parquet/manifest
   metadata (A2 work) and place data files' internals via the content-dependent
   engine, so Iceberg scans get intra-object pushdown for free.

This branch is for this work, separate from the placement-engine branches.

## Evaluation log

### 2026-10-24 — Layer 1 (warehouse/FileIO): PASS on distributed WarpDrive
A real pyiceberg client ran end to end against the distributed RS(3,2) cluster
via /cluster/s3: create table, append 1000+200 across two snapshots, full scan
(1200), filtered scan region==r3 (150), time-travel to snapshot[0] (1000). 9
objects written (2 parquet, 3 metadata.json, 4 avro manifests). See
eval_pyiceberg.py.

Compatibility findings (the "parser and other problems"):
1. Must use fsspec/s3fs FileIO, not pyiceberg's default pyarrow FileIO: pyarrow
   S3FileSystem takes only host:port and cannot target the /cluster/s3 path
   prefix. => Real gap: the distributed engine is reachable only at /cluster/s3,
   which pyarrow-based clients can't hit. The clean fix is Layer-2 work: expose
   the distributed S3 surface at the standard root path so ANY client works.
2. Single-node /s3 does not decode aws-chunked (streaming-signed PUT) bodies —
   it stored chunk framing into objects, corrupting metadata.json on read-back.
   The distributed coordinator decodes correctly. (Known latent single-node gap.)
3. Local SqlCatalog avoids needing S3 conditional-write commit semantics; a
   native REST catalog (Layer 2) will need to handle commit atomicity itself.

### 2026-10-24 — Layer 1b: distributed engine now served at the ROOT S3 path
main.rs: in cluster mode (WARPDRIVE_PEERS set), the root S3 surface
(/{bucket}/...) routes object + list-objects ops to the DISTRIBUTED engine's
authenticated handlers (register_root_s3). Standard clients connect at the
normal endpoint with no /cluster/s3 prefix. Confirmed: boto3 create/put/head/
get/list at root resolve to the same distributed object visible via /cluster/s3;
a full pyiceberg lifecycle (create/append x2/scan/filter) works at the root
endpoint. Bucket-registry ops stay shared; single-node keeps the in-process
store.

Open S3-compat gap (next fix): pyarrow's S3 client PUTs with
`x-amz-content-sha256: STREAMING-UNSIGNED-PAYLOAD-TRAILER` (unsigned chunked +
CRC32 trailer). That framing is not decoded on at least one write path (small
PUT and/or multipart part), so pyarrow's default FileIO stores corrupt bytes;
s3fs/boto3 (non-chunked) work. decode_aws_chunked_payload itself handles
unsigned headers, so the miss is a path that doesn't invoke it (suspect the
multipart part handler). Until fixed, use the fsspec FileIO for pyiceberg.

### 2026-10-24 — Fix: decode aws-chunked on multipart part uploads
pyarrow's S3 client uploads every object as a multipart part with
`x-amz-content-sha256: STREAMING-UNSIGNED-PAYLOAD-TRAILER` (unsigned chunked +
trailer). The cluster part-upload path passed the body raw, storing the chunk
framing. Fix: decode in cluster_s3_put_object_authenticated's part branch
(decode_chunked_if_streaming -> the now-pub(crate) decode_aws_chunked_payload,
which already handles unsigned headers and ignores the trailer). Verified:
pyarrow writes a 50KB object (multipart, unsigned-chunked) and boto3 reads it
back byte-identical (sha match); a full pyiceberg lifecycle works via the
default pyarrow FileIO.

Next gap (separate, client-side): the AWS SDK's 2025 default data-integrity
checksums (crc64nvme) make pyarrow's GET fail with "Response checksums mismatch"
because WarpDrive doesn't return a matching checksum header. Workaround today:
clients set AWS_RESPONSE_CHECKSUM_VALIDATION=when_required (and
AWS_REQUEST_CHECKSUM_CALCULATION=when_required). Full out-of-box support means
computing/storing/returning object checksums (crc32/crc64nvme) server-side.

### 2026-10-24 — Fix: honor client checksum algorithm end-to-end (CLOSES the gap)
The distributed path hardcoded CRC32C on write and echoed the whole-object
checksum even on partial (206) responses. Fixes: (1) parse the client's declared
algorithm from x-amz-sdk-checksum-algorithm / x-amz-checksum-algorithm /
x-amz-trailer; (2) single PUT computes the declared algorithm server-side when
the value only arrives in the chunked trailer; (3) multipart captures the
algorithm per part (pyarrow declares it on the UploadPart trailer, not at
initiate) and complete checksums the reassembled object with it; (4) GET no
longer echoes the whole-object checksum on partial (206) bodies — mirrors AWS,
which omits x-amz-checksum-* on ranged GETs, so a ranged Parquet read no longer
sees a whole-object checksum it would reject. Verified: pyiceberg now works via
BOTH the native pyarrow FileIO and fsspec, no checksum env var or workaround
(eval_pyiceberg.py runs both). This retires finding #1's pyarrow limitation.
Layer 1 is DONE.

## Layer 2 — Iceberg REST catalog (in progress)
Goal: serve the Iceberg REST Catalog protocol so external engines (pyiceberg
RestCatalog, Spark, Trino) connect to WarpDrive directly instead of each client
keeping its own SqlCatalog. Metadata pointer and catalog state live in WarpDrive.
Endpoint set (minimum for pyiceberg RestCatalog): GET /v1/config; namespaces
(list/create/load/exists/drop); tables (list/create/load/exists/drop/rename) and
the atomic commit (POST /v1/namespaces/{ns}/tables/{tbl}). The hard part is
commit atomicity — a compare-and-swap on each table's current metadata pointer —
which the local SqlCatalog got from SQLite and a native catalog must provide
itself.

### 2026-10-24 — Layer 2 (REST catalog): PASS, commit atomicity from WarpDrive CAS
rest_catalog.py serves the Iceberg REST protocol with its authoritative state
living ENTIRELY in WarpDrive — no external catalog DB:
  * namespace markers  -> __catalog__/namespaces/<ns>.json
  * table pointers     -> __catalog__/tables/<ns>/<tbl>.json = {metadata_location, version}
  * table metadata.json -> standard Iceberg layout under the table location
Iceberg metadata semantics (build initial metadata, validate a commit's
requirements, apply its updates, serialize metadata.json) are reused from
pyiceberg itself, so the service is only protocol + storage + atomicity.

Commit atomicity is a compare-and-swap on the pointer OBJECT via WarpDrive's S3
conditional writes (verified end to end, ck_cas.py): create = PUT If-None-Match:*
(409 if present), commit = PUT If-Match:<etag> (409 if another commit won). This
mirrors SqlCatalog's `UPDATE ... WHERE metadata_location=<current>` CAS, but the
"row" is a WarpDrive object and the lock is the 412 the object store returns — so
the catalog needs no database and is effectively stateless. The If-Match (not the
server's in-process lock) is what guarantees correctness: a loser always reloads
from whatever the pointer currently points to before retrying.

Evaluated (eval_rest_catalog.py):
  A. a real pyiceberg RestCatalog given ONLY the REST uri (S3 wiring via
     /v1/config overrides) — create namespace/table, append x2, full/filtered/
     time-travel scans, reload-from-catalog. PASS.
  B. atomicity — TWO independent catalog processes (ports 8181/8182, no shared
     lock) racing concurrent commits on one table: 30 concurrent appends, ~15
     real 409 conflicts detected and retried, 31 distinct snapshots, ZERO lost
     updates. The only serialization is WarpDrive's conditional-PUT CAS. PASS.

Known gaps (follow-ups, not correctness): orphaned metadata.json left by a losing
commit accumulates (same GC class as the rest of the engine); rename_table and
update_namespace_properties not yet implemented; staged-create (create-table
transaction) returns a clear error rather than reconstructing base metadata.

### Layer 3 — placement bridge (in progress)
The differentiator: place a Parquet data file's column chunks via the
content-dependent engine so a selective scan fetches only the columns it needs
(intra-object pushdown) — something S3 Tables, which optimizes at the whole-file
level (compaction/clustering), structurally cannot do.

#### 2026-10-24 — direct S3-level proof + read telemetry
Added per-GET read telemetry to the cluster GET path: response headers
`x-warpd-stripes-fetched`/`-total` and `x-warpd-shard-bytes-read` (bytes read off
the shard backing store to serve the request) on both the content-dependent and
plain paths — the honest intra-object-pushdown metric, now client-visible.

Unit model (eval_pushdown.py): tile a Parquet exactly; each column chunk is split
into K=3 contiguous sub-units sharing one group id = the column index. The
`ivf_centroid` group-by-key packer then places all K in ONE stripe (one per data
bin, so the stripe is balanced ≈ parity-only overhead) AND isolates the column to
a single stripe. The footer gets its own tiny stripe (every scan reads it).

Measured, 8-column single-row-group Parquet (1.42 MB), RS(3,2), 1-column projection:
  * plain (opt-out, positional k-split): 946,422 bytes read off disk (2 of 3
    shards — the column's byte range + the footer each drag a full 473 KB shard)
  * content-dependent (column-grouped):   181,638 bytes (the column's own stripe
    179,937 + an isolated 1,701-byte footer stripe), touching 1 of 10 stripes
  * => 5.2x less data read off disk for the projection, at 0.0027% storage
    overhead; full GET byte-identical to the original (exact tiling, no corruption).

Degrade-to-plain safety confirmed working: one-unit-per-column (unbalanced,
K-1 padded bins) reported ~3x overhead and auto-fell-back to plain under a 60%
budget; the K-split balanced layout reports 0.0027% and engages.

Next: a custom pyiceberg FileIO that derives these units from each data file's
footer so Iceberg tables get this transparently; then measure a real Iceberg
projection scan and run Spark/Trino against the REST catalog.
