# 2026-11-26, setup C: the metadata write path PR

5 x n2-standard-2 (2 vCPU, 8 GB, local NVMe) and a client n2-standard-2, RS(3,2), the same VMs as
`../2026-11-23-gcp-rs32`. Hardware and software per VM: `environment/` (captured after the sweep).

| Folder | Question | Answer |
|---|---|---|
| `size-sweep-write-path/` | What does `feat-s3-write-path` (stacked on #244) do from 4 KB to 256 MB? | +19% PUT and +9% GET at 4 KB, +13% PUT and +19% GET at 64 KB, a tie from 1 MB up |
| `size-sweep-byte-path/` | What do hardware CRC32C, SIMD Reed-Solomon, assembly MD5 and hashing during the write (`feat-s3-byte-path`, stacked on the write path PR) add? | PUT +6% at 4 KB, +19 to 31% at 64 KB, +27% at 1 MB, +9% at 8 MB, +17% at 64 MB. GET unchanged from 1 MB up. 256 MB within noise (reruns) |
| `profile-256mb/` | Where a 256 MB PUT's CPU goes on both builds | CRC32C 21% to 2%, Reed-Solomon 15% to 5%. MD5 (26%) and page faults on fresh buffers (19%) are what's left. Node 0 under one core busy |
| `profile-8mb/` | What limits objects of 1 MB and up? | Per-byte work on node 0: on PUT CRC32C 23%, MD5 25%, Reed-Solomon 14%, copies 21%. On GET copies, and every GET fetches all four remote shards |

Scripts (`scripts/`): `sweep.sh` (the sweep driver, binaries from `A_BIN`/`B_BIN`), `prof8.sh` and `profx.sh` (the profile drivers), `netmon.sh` (network and disk
counters), `plot_sweep.py`, `env_capture.sh`.
