# GCP RS(3,2), 2026-11-23

5 x `n2-standard-2` nodes (2 vCPU, 8 GB, local NVMe ext4) and the client `wlc` resized to `n2-standard-2`, us-central1-a.
12 vCPUs, the project's cap. Hardware and software per VM in `environment/`. Reconfigured from the RS(2,1) setup with
`scripts/reconf32.sh`.

| Folder | Question |
|---|---|
| `size-sweep` | Bitcask against RocksDB with #238, 4 KB to 64 MB objects: the last experiment before the release |
