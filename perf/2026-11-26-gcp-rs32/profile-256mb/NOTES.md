# 256 MB PUT: the write path PR against the per-byte PUT work PR

Question: the sweep showed fewer 256 MB PUTs for the per-byte PR in 60 s runs. Is that a slowdown, and where does a
256 MB PUT's CPU go on each build?

Builds: `prof256a` = `feat-s3-write-path` (`wd-pr`), `prof256b` = `feat-s3-byte-path` (`wd-prb`), run in that order.
Setup C, RS(3,2), RocksDB. 256 MB x 2 clients from `s3_bench` on `wlc`: 90 s PUT then 90 s GET. Profiles as
`../profile-8mb` (perf record 15 s at +25 s on node 0, node 1 and the client, perf stat 10 s). Driver
`../scripts/profx.sh`. 0 errors.

| | write path PR | per-byte PR |
|---|---|---|
| PUTs in 90 s (p50) | 24 (6.5 s) | **28 (5.4 s)** |
| GETs in 90 s | 44 | 53 |
| Node 0 cores busy (perf stat) | 0.96 | 0.87 |

## Node 0 during PUT (flat profile)

| | write path PR | per-byte PR |
|---|---|---|
| CRC32C | 20.8% (`crc` crate, table) | **1.8%** (`crc32c` `crc_u64_parallel3`) |
| Reed-Solomon | 14.8% (`galois_8` Rust) | **4.6%** (`reedsolomon_gal_mul*`, SIMD) |
| MD5 | 22.0% (Rust `transform` + `consume`) | 26.3% (`md5_compress`, assembly) |
| Page faults and zeroing fresh pages | 11.4% (`clear_page_erms`, `do_user_addr_fault`) | 18.8% |
| By library: WarpDrive / kernel / libc | 59 / 32 / 9% | 34 / 52 / 14% |

## Findings

- **The per-byte work is gone except MD5**: CRC32C from 21% to 2%, Reed-Solomon from 15% to 5%. The assembly MD5 is
  no faster than the Rust one here, and MD5 is now the largest single cost.
- **Next is memory**: about a fifth of node 0's CPU goes to faulting in and zeroing freshly allocated pages for each
  request's buffers. Reusing buffers would remove it.
- **Node 0 isn't the limit at 256 MB**: it uses under one of two cores. Two clients and the client's own SHA-256 set
  the rate, which is why 60 s runs at this size swing by several requests.
