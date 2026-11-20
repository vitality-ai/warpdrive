# GCP core count and CPU cost, 2026-11-21

Follow-up to `../2026-11-20-gcp-4kb/`. The saturation sweep showed the coordinator's 2 vCPUs as the limit for 4 KB requests.

| Folder | Question | Answer |
|---|---|---|
| `cores/` | Does node0 go faster with 4 cores than with 2? | RS(2,1): yes, by 32-41%. Single node: no, it is not CPU-bound. |
| `cpu-trace/` | Why does each request cost so much CPU? | About 85-90% is thread hand-offs, gRPC to peers, allocation and HTTP overhead. The storage work is 5-15%. |

Setup: node0 n2-standard-4, nodes 1-2 n2-standard-2, all with local NVMe. The client `wlc` is n2-standard-4 and stays up between experiments.
Build: #226 at b62e763. Scripts in `scripts/`.
