# GCP 4 KB benchmarks, 2026-11-20

Small-object (4 KB) PUT and GET on a 5-node RS(3,2) cluster, comparing #217 (RFC 5 one-object service) with #226
(cluster ordered writes and S3 parity), each with and without inline objects.

## Setup

- 5 x GCP `n2-standard-2` (2 vCPU, 8 GB) with local NVMe, plus 1 x `n2-standard-2` load client, us-central1-a.
- Every request goes to node0. Logging at WARN. Blob cache 256 MB.
- Scripts used are in `scripts/` (`longrun.sh` driver, `sampler.sh` per-node CPU/NVMe/RSS, `export.py` and `sweep_plot.py` plots).

## Runs

| Folder | What | Builds |
|---|---|---|
| `run1-before-fix/` | 10 min PUT then 10 min GET, 32 clients | #217 23ef9dd, #226 f1152d4..3af7dfe |
| `run2-after-fix/` | same, after the cache and HTTP-worker fix | #217 23ef9dd, #226 b62e763 |
| `sweep/` | 32 to 512 clients, 120 s per phase | #217 23ef9dd, #226 b62e763 |

Each folder has `summary.csv`, the plots, and a `NOTES.md` where a result needs a caveat.

## Findings

1. **Run 1 found a #226 regression.** GET was 410/s against #217's 3,197/s and decayed over the run.
   `perf` on node0 showed an O(n) scan in the blob cache LRU and only 2 actix workers capping PUT.
2. **The fix (b62e763)** made the cache LRU O(1) and set HTTP workers to at least 32. In run 2, #226 PUT matches #217
   (1,017 vs 1,001/s) and GET rose 3.6x to 1,474/s, still below #217's 2,553/s.
3. **#217 with inline 8 KB** is the fastest (1,718 PUT/s, 14,535 GET/s) but holds rows in memory: node RSS reached
   7.5 GB of 8 GB in 10 minutes and ran out of memory in run 1. Inline by default needs an engine that keeps values on disk.
4. **#226 with inline 8 KB in run 2 is invalid.** node0 began resetting connections after about 6 minutes. Cause not found.
5. **At saturation the coordinator's CPU is the limit** for both builds (node0 92-99% busy, NVMe at most 40%).
   #226 PUT scales to 1,225/s at 512 clients with no errors, while #217 collapses to 452/s with 85,757 errors.
   #226 GET stays about 20% below #217 at every client count.

## Next

- Profile #226 GET on node0 to close the gap with #217.
- Reproduce the #226 inline failure with server logs kept.
- Spread requests over all nodes so the cluster, not node0, is measured.
