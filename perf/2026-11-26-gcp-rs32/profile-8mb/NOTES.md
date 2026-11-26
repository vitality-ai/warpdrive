# Where an 8 MB object's CPU goes

Question: from 1 MB up, PUT runs at about 140 MB/s and GET at about 330 MB/s on this cluster. What limits them?

Build: `feat-s3-write-path` at 5476e91 (`wd-pr`). Setup C, RS(3,2), RocksDB. 8 MB objects x 16 clients from `s3_bench`
on `wlc` to node 0: 90 s PUT then 90 s GET (18.1 PUT/s, 39.8 GET/s, 0 errors). In each phase at +25 s:
`perf record -F 99 --call-graph dwarf` for 15 s on node 0, node 1 and the client, then `perf stat` and `pidstat` for 10 s
on node 0. Network and disk counters every 2 s on node 0, node 1 and the client. Driver `../scripts/prof8.sh`. Flat, by
library and inclusive reports and flame graphs in `raw/`.

## PUT, node 0 (1.77 of 2 cores busy)

| Where | Share | What it is |
|---|---|---|
| CRC32C of the body | 23% | `s3::handlers::checksum::compute_checksum` with the `crc` crate: a table lookup per byte. The CPUs have SSE4.2, which computes CRC32C in hardware |
| MD5 of the body (ETag) | 25% | `md5` 0.7 (`transform` + `consume`) |
| Reed-Solomon encode | 14% | `reed-solomon-erasure` without `simd-accel`: `galois_8::mul_slice_xor` and `mul_slice` |
| memcpy and kernel copies | about 21% | libc memcpy, `rep_movs_alternative`, `clear_page_erms` (zeroing fresh pages) |
| Disk | under 1% | 32 fdatasync/s, NVMe about 3% busy |

## GET, node 0 (1.66 cores busy)

55% of the CPU is in libc (memcpy) and 38% in the kernel (copies, page clearing, receive path). WarpDrive's own code is
6%. `recvfrom` runs 32,800 times a second for about 330 MB/s, so shards arrive in reads of about 6 KB.

## Client

89% of the client's CPU on PUT is SHA-256 (`sha2::sha256::compress256`) of the payload for SigV4. The CPUs (Intel Xeon
2.8 GHz, n2) have no SHA-NI. On GET the client is mostly copying.

## Network (measured, `raw/*-net.csv`, bytes on the NIC averaged over each phase)

| | node 0 in | node 0 out | node 1 in | node 1 out | client in | client out |
|---|---|---|---|---|---|---|
| PUT | 154 MB/s | 205 MB/s | 51 MB/s | 0 | 0 | 154 MB/s |
| GET | 447 MB/s | 334 MB/s | 0 | 112 MB/s | 331 MB/s | 0 |

The busiest direction is node 0's GET intake, 447 MB/s (3.6 Gbps) of the n2-standard-2's 10 Gbps. Not saturated.

**GET fetches every shard, parity included.** To serve 334 MB/s, node 0 takes in 447 MB/s (1.34 times the object
bytes). With RS(3,2) it holds one shard itself and needs two more (0.67 times). Node 1 alone sends 112 MB/s, a quarter
of node 0's intake, so each of the four other nodes sends its shard. Open PR #171 addresses this.

## What would make large objects faster

1. CRC32C in hardware (the `crc32c` crate, same values): about 23% of PUT CPU.
2. Reed-Solomon with SIMD (`reed-solomon-erasure` `simd-accel`): up to 14%.
3. A faster MD5 (assembly), or MD5 on the other core while the encode runs.
4. Fetch only the shards a GET needs (two of four remote ones here, #171), fewer copies, and larger reads of each
   shard.
5. Requests spread over all five nodes: nodes 1 to 4 are at 5 to 13% CPU.
6. For benchmarks: `UNSIGNED-PAYLOAD` signing in `s3_bench`, so the client stops being a limit.
