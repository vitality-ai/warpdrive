# 2026-11-26, setup C: the metadata write path PR

5 x n2-standard-2 (2 vCPU, 8 GB, local NVMe) and a client n2-standard-2, RS(3,2), the same VMs as
`../2026-11-23-gcp-rs32`. Hardware and software per VM: `environment/` (captured after the sweep).

| Folder | Question | Answer |
|---|---|---|
| `size-sweep-write-path/` | What does `feat-s3-write-path` (stacked on #244) do from 4 KB to 256 MB? | +19% PUT and +9% GET at 4 KB, +13% PUT and +19% GET at 64 KB, a tie from 1 MB up |
| `profile-8mb/` | What limits objects of 1 MB and up? | Per-byte work on node 0: on PUT CRC32C 23%, MD5 25%, Reed-Solomon 14%, copies 21%. On GET copies, and every GET fetches all four remote shards |

Scripts (`scripts/`): `sweep.sh` (the sweep driver), `prof8.sh` (the profile driver), `netmon.sh` (network and disk
counters), `plot_sweep.py`, `env_capture.sh`.
