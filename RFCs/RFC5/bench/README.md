# RFC 5 benchmark setup

Reproduces the numbers in `../results/`.

**Hardware (GCP, us-central1-a):** 5 × `n2-standard-2` cluster nodes, each with its own 375 GB local NVMe SSD (data at `/mnt/nvme`), plus 1 × `n2-standard-2` client. Ubuntu 22.04. All traffic stays inside the zone's internal network.

**Cluster:** RS(3,2), one `warp_drive` per node, gRPC shard transport (the default). The client sends every request to node 0, which coordinates.

| Script | Runs on | What it does |
|---|---|---|
| `node-setup.sh` | each node | formats and mounts the local NVMe, installs runtime libs |
| `node-tools.sh` | each node | installs `sysstat` and `perf` |
| `client-setup.sh`, `client-build.sh` | client | Rust toolchain; builds `main` and the prototype into separate target dirs |
| `node.sh` | each node | `start <binary> <run> [env]` wipes data and starts the server plus `sar`/`iostat`. `stop <run>` collects traces, timings and monitor output. Peer IPs are hard-coded at the top. |
| `bench.sh` | client | runs the matrix: variants × sizes × trials, the concurrency sweep, and two `perf` profiles |

**Variants in `bench.sh`:** B0 = `main`. B1 = this branch at its defaults (the redundant local sync, T2, is skipped whenever `WARPDRIVE_SELF_ADDR` is in the peer list, which `node.sh` sets). B2 = B1 + `WARPDRIVE_INLINE_MAX_BYTES=65536` (T1).

**The 2026-11-19 results in `../results/`** came from commit 783501f, where each technique had its own switch: B1 = every switch off, B2 = T1 only, B3 = T1 + T2. That B3 is today's B2, and today's B1 is T2 alone, which wasn't measured on its own.

**Logging:** `node.sh` copies the `server_log.yaml` in `~/bins`. Headline runs use the shipped default (`info`, no per-request lines). The per-request stage traces (`cluster PUT ... shards_us=`) are at `trace`. Collect them in separate runs with the `warp_drive` logger at `trace`, never in headline runs.

**Each run:** fresh data directories, PUT phase for 10 s, then GET phase for 10 s over the keys just written, concurrency 16 unless noted. `s3_bench` signs every request with SigV4 and records each request's latency.

Analysis: `python3 ../analyze.py <results_dir> <out_dir>`.
