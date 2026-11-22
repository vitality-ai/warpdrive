# GCP core count and CPU cost, 2026-11-21

Follow-up to `../2026-11-20-gcp-4kb/`. The saturation sweep showed the coordinator's 2 vCPUs as the limit for 4 KB requests.

| Folder | Question | Answer |
|---|---|---|
| `cores/` | Does node0 go faster with 4 cores than with 2? | RS(2,1): yes, by 32-41%. Single node: no, it is not CPU-bound. |
| `cpu-trace/` | Why does each request cost so much CPU? | About 85-90% is thread hand-offs, gRPC to peers, allocation and HTTP overhead. The storage work is 5-15%. |
| `handoff-ab/` | Does skipping the bridge-thread hop help? Where do the context switches come from? | No measurable change: that hop is 1 of 30 switches per PUT. About 350 threads in four pools share node0's 2 cores. |
| `async-meta-ab/` | Does awaiting the metadata write instead of blocking the HTTP worker help? | No: PUT -3 to -5%, CPU per request unchanged. The cost moves to the bridge runtime. Crossing runtimes is the cost. |

Setup: node0 n2-standard-4, nodes 1-2 n2-standard-2, all with local NVMe. The client `wlc` is n2-standard-4 and stays up between experiments.
Build: #226 at b62e763. Scripts in `scripts/`.
| `regression-main-vs-226/` | Does #226 regress anything against main? | PUT faster or equal everywhere, 1 MB cluster GET +50%. 4 KB GET -25% cluster, -6% single node. |
| `async-get-ab/` | Does awaiting shard reads on the HTTP worker fix the cluster GET gap? | +12% (4,766 to 5,341 GET/s), half the gap. Now the default in #226. |
