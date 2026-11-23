# Compaction run 2: fixed sweep against gc/ rows

Question: with the sweep's stalls fixed, which free path is better under a steady overwrite load: dead shards found by a
sweep every minute (confirmed with peers, freed after two sweeps), or writes recording the blobs they kill in gc/ rows
(freed past the grace period, no scan)?

Builds: sweep beab212 (385afd3 batched confirmations + chunked scans, cherry-picked), gc 9985702 (#224 then: gc/ rows,
chunked scans; sweep as a daily audit only). Both punch and use copy_file_range. RS(2,1), 64 MB segments, grace 120 s,
sweep (sweep arm) every 60 s, gc pass every 60 s. Servers started with `ulimit -n 1048576` (the #229 fix came after).
10 min of overwrites then 10 min of reads. Driver `scripts/comp2.sh`.

| Workload | Arm | PUT/s | PUT p99 | Disk at end of writes | Peak | At end | CPU during writes |
|---|---|---|---|---|---|---|---|
| 4 KB, 100,000 keys, 128 clients | sweep | 2,784 | 244 ms | 2.27 GB (11.1x) | 2.65 GB | 0.21 GB (1.02x) | 69% |
| | gc/ | 2,795 | **99 ms** | **1.54 GB (7.5x)** | **1.96 GB** | 0.26 GB (1.25x) | 80% |
| 1 MB, 1,000 keys, 32 clients | sweep | 229 | 221 ms | 35.9 GB (69x) | 36.4 GB | 0.53 GB (1.00x) | 41% |
| | gc/ | 224 | 228 ms | **27.7 GB (53x)** | **30.2 GB** | 0.53 GB (1.01x) | 43% |
| 256 MB, 20 keys, 2 clients | sweep | 0.38 | 4.7 s | 12.3 GB (4.6x) | 15.3 GB | 2.68 GB (1.00x) | 16% |
| | gc/ | 0.38 | 4.6 s | 12.0 GB (4.5x) | 14.9 GB | 2.68 GB (1.00x) | 16% |

Live data per node: 0.205 GB (4 KB), 0.524 GB (1 MB), 2.684 GB (256 MB). 0 PUT and 0 GET errors in every arm.

## Findings

1. **gc/ rows keep disk lower during writes** (about a third less at the end of writes and at the peak, 4 KB and 1 MB)
   and keep 4 KB PUTs steady: the sweep arm dips to 1,800-2,300/s during each sweep, and its overall PUT p99 is 244 ms
   against 99 ms.
2. **gc/ rows cost CPU at 4 KB (69% to 80%)**: every overwrite also writes a replicated gc/ row, roughly doubling the
   replicated metadata writes per overwrite. One gc/ row per group commit instead of per write would cut it.
3. **Both get back to the live data** once writes stop: 1.00 to 1.25x.
4. **256 MB: dead shards free without any copy or punch.** Each 128 MB shard has a segment file of its own, so a dead
   one's segment is deleted whole: copied 0, punched 0, end exactly at live.
5. **Headroom during writes is set by the grace period.** Data can be freed only once it is older than the grace (120 s
   here) plus up to a pass interval, so a node needs about write rate x (grace + 1 to 2 min) of free disk: at 1 MB,
   about 114 MB/s per node, that is the 27-36 GB seen. With the default grace of 600 s it would be about 80 GB.
