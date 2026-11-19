# RFC 5 benchmark setup

Reproduces the numbers in `../results/`.

**Hardware (GCP, us-central1-a):** 5 × `n2-standard-2` cluster nodes, each with its own 375 GB local NVMe SSD (data at `/mnt/nvme`), plus 1 × `n2-standard-2` client. Ubuntu 22.04. All traffic stays inside the zone's internal network.

**Cluster:** RS(3,2), one `warp_drive` per node, gRPC shard transport (the default). The client sends every request to node 0, which coordinates.

| Script | Runs on | What it does |
|---|---|---|
| `node-setup.sh` | each node | formats and mounts the local NVMe, installs runtime libs |
| `node-tools.sh` | each node | installs `sysstat` and `perf` |
| `client-setup.sh`, `client-build.sh` | client | Rust toolchain; builds `main` and the prototype into separate target dirs |
| `node.sh` | each node | `start <binary> <run> [env]` wipes data and starts the server plus `sar`/`iostat`; `stop <run>` collects traces, timings and monitor output |
| `bench.sh` | client | runs the matrix: variants × sizes × trials, the concurrency sweep, and two `perf` profiles |

**Variants:** B0 = `main`. B1 = prototype binary with every switch off. B2 = B1 + `WARPDRIVE_INLINE_MAX_BYTES=65536`. B3 = B2 + `WARPDRIVE_SKIP_LOCAL_CLEANUP=1`.

**Each run:** fresh data directories, PUT phase for 10 s, then GET phase for 10 s over the keys just written, concurrency 16 unless noted. `s3_bench` signs every request with SigV4 and records each request's latency.

Analysis: `python3 ../analyze.py <results_dir> <out_dir>`.
