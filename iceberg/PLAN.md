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
