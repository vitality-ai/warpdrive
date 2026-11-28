# Compiling for the CPU: default, native and x86-64-v3 builds

Question: does building WarpDrive (and the zstd and RocksDB C code in it) for this CPU make compression or the
rest of the PUT and GET path faster?

Builds, all from `feat-pluggable-frontends` at 4891e3f (#250):

| Build | Rust | C and C++ |
|---|---|---|
| `wd-base` | default (x86-64) | default |
| `wd-nat` | `-C target-cpu=native` | `-O3 -march=native` |
| `wd-v3` | `-C target-cpu=x86-64-v3` | `-O3 -march=x86-64-v3`, C++ also `-mpclmul` (RocksDB's CRC32C needs it once AVX2 is on) |

`native` here includes AVX-512: every VM is an Intel Xeon at 2.8 GHz with AVX-512 and BMI2 (Cascade Lake class).
`x86-64-v3` is AVX2, BMI2 and FMA without AVX-512.

Setup: 5 x n2-standard-2 (local NVMe), RS(3,2), RocksDB metadata (1 GB cache), fresh cluster per run, every request
to node 0. `s3_bench` (00e9dde7) on `wlc`, text data (JSON lines, zstd keeps about a fifth). 60 s PUT then 60 s
GET. Two runs per cell, the order of the six build and arm pairs rotated between runs. Driver `../scripts/zs3.sh`
(stage `native`), derived numbers `derived.csv` and `means.csv` (`../scripts/derive3.py`). 0 errors in every run.

Mean of two runs, ops/s (min to max in `means.csv`):

| Build | 64 KB PUT off / zstd | 64 KB GET off / zstd | 1 MB PUT off / zstd | 1 MB GET off / zstd |
|---|---|---|---|---|
| default | 1,086 / 1,042 | 1,826 / 1,910 | 176.5 / 176.1 | 342 / 424 |
| native | 993 / 960 | 1,746 / 1,857 | 177.1 / 167.8 | 323 / 401 |
| x86-64-v3 | 1,049 / 1,093 | 1,743 / 2,102 | 176.2 / 175.9 | 341 / 426 |

## Findings

- **Native is slower or the same everywhere**: PUT −8% at 64 KB, GET −5% at 1 MB, zstd PUT −5% at 1 MB. These
  CPUs lower their clock while running AVX-512 code, and `target-cpu=native` lets the compiler use it anywhere.
- **x86-64-v3 helps only zstd at 64 KB**: GET +10% (the two runs don't overlap) and PUT +5% (they overlap). Every
  other cell is within run-to-run variation.
- Node 0's CPU is 85 to 98% busy in every 64 KB cell, so the gains a faster zstd could bring are capped by
  everything else node 0 does per request.

## Decision

Release binaries stay on the default build. The full sweep (`../sweep`) uses it too. A v3 build could be offered
later for zstd users, measured again then.
