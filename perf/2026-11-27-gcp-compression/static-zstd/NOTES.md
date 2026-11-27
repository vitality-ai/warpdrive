# Blob compression, static: off against zstd on every blob

Question: does compressing blob bytes with zstd before erasure coding make writes and reads faster, by moving fewer
bytes between nodes?

Build: `feat-pluggable-frontends` with the zstd stage (`wd-z`, a42a249d), one binary for both arms:
`WARPDRIVE_BLOB_COMPRESSION` unset (off) or `zstd` (level 1, every blob that shrinks by at least a tenth).
Setup C: 5 x n2-standard-2 (local NVMe), RS(3,2), RocksDB metadata (1 GB cache), fresh cluster per run, every request
to node 0. `s3_bench` (`s3b-z`, 307f9b6c) on `wlc`: 60 s PUT then 60 s GET. Payloads (`S3B_DATA`): `text` (JSON
lines, zstd level 1 keeps about a fifth) and `random` (incompressible). Network: bytes sent by all five nodes in the
phase (`netmon.sh`) over ops. CPU: mean busy % per node in the phase. Driver `../scripts/zs.sh`, derived numbers
`derived.csv` (`../scripts/derive.py`). One run per cell, 0 errors everywhere.

| Data, size | PUT/s off → zstd | GET/s off → zstd | Sent per PUT off → zstd | Node 0 CPU on PUT off → zstd |
|---|---|---|---|---|
| text 64 KB | 1,177 → 979 (−17%) | 2,045 → 1,760 (−14%) | 89 → 23 KB | 97 → 97% |
| text 1 MB | 177 → 175 (−1%) | 333 → 421 (**+27%**) | 1,348 → 308 KB | 72 → 94% |
| text 8 MB | 19.1 → 16.4 (−14%) | 39.9 → 54.2 (**+36%**) | 11.1 → 2.2 MB | 53 → 62% |
| random 64 KB | 1,091 → 994 (−9%) | 1,808 → 1,686 (−7%) | 90 → 90 KB | 97 → 97% |
| random 1 MB | 177 → 177 | 333 → 332 | same | 72 → 75% |
| random 8 MB | 19.3 → 17.5 (−9%) | 40.0 → 39.7 | same | 55 → 54% |

Disk used per node after the 64 KB text run: 1.58 GB off, 0.28 GB zstd (the zstd arm wrote 17% fewer objects).

## Findings

- **No setting wins everywhere.** zstd cuts the bytes a text PUT sends by three quarters or more, but costs PUT
  throughput at every size: node 0 compresses every blob, and at 64 KB node 0 is already at 97% CPU.
- **Reads gain from 1 MB up**: +27% at 1 MB, +36% at 8 MB. A GET fetches the blob's shards to node 0, and compressed
  shards are a fifth of the size. The client still gets the full object.
- **Incompressible data only pays**: one failed attempt per blob, −9% PUT at 64 KB and 8 MB.
- **The other four nodes get cheaper** with text: their CPU on PUT falls from 15-34% to 2-28%.

## Decision

Compression doesn't stay a fixed setting. Whether it pays depends on what's short at the moment (node 0's CPU, the
network, disk space) and on how often blobs are read. So it becomes a policy that decides per blob from
measured load (`auto`, `cluster/adaptive.rs`). It's measured against both static settings in `../auto`, and becomes
the default only if it never does worse than off there.
