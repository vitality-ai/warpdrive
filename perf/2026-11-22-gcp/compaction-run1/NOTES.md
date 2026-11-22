# Compaction run 1: the first compactor, sweep-driven

Question: under a steady overwrite load, does the compactor keep disk near the live data, and what does it cost the
foreground? Does punching dead blocks plus `copy_file_range` beat compaction alone?

Builds (on #226 async): new 44a4055 (segments, two-sweep confirmation, compaction, punching, `copy_file_range`), old
af69266 (compaction only, read+write copy), off (44a4055 with `WARPDRIVE_COMPACT=off`: no freeing, sweeps still run).
RS(2,1), 64 MB segments, grace 120 s, sweep every 60 s, confirm gap 60 s, seal idle 120 s. 10 min of overwrites then 10
min of reads. Driver `scripts/comp.sh`; summary and per-minute series from `scripts/analyze_comp.py`.

| Workload | Arm | PUT/s | PUT p99 | Disk at end of writes | At end | PUT errors |
|---|---|---|---|---|---|---|
| 4 KB, 100,000 keys, 128 clients | new | 1,648 | 597 ms | 2.02 GB (9.9x live) | 1.23 GB (6.0x) | 0 |
| | old | 1,674 | 619 ms | 2.06 GB (10.0x) | 1.26 GB (6.1x) | 0 |
| | off | 1,636 | 633 ms | 2.01 GB (9.8x) | 2.01 GB | 0 |
| 1 MB, 1,000 keys, 32 clients | new | 227 | 220 ms | 32.9 GB (63x) | 0.53 GB (1.00x) | 0 |
| | old | 257 | 187 ms | 39.8 GB (76x) | 0.58 GB (1.11x) | 0 |
| | off | 208 | 219 ms | 64.0 GB (122x) | 64.0 GB | 19,892 |

Live data per node: 0.205 GB (4 KB), 0.524 GB (1 MB). GET errors: 0 in every arm.

## Findings

1. **Sweeps stalled 4 KB PUTs in every arm**, off included: from about 3,700/s to about 400/s (p99 about 1.3 s) while a
   sweep ran, with CPU falling. Each peer answered a confirmation request with a full scan of its metadata rows under
   the index's read lock, and parking_lot queues readers behind a waiting writer. Fixed in 9985702 (chunked scans).
   At 1 MB (1,000 keys, few rows) sweeps cost PUTs nothing.
2. **A sweep took about 10 minutes** at 900,000 shards per node (700 full scans per peer, one per 1,000-shard request),
   and dropping dead entries one fsync each took about 2 minutes. Freeing needs two sweeps, so disk ran to 33-40 GB
   per node against 0.5 GB live during the 1 MB writes. Fixed in 385afd3 (20,000-shard requests, one sync per batch),
   and replaced as the free path by gc/ rows in 01abbb7 (the sweep becomes a daily audit).
3. **Once writes stop, both compactors free everything**: 1 MB ends at 1.00x live with punching and copy_file_range,
   1.11x with compaction alone, with a lower peak (39.6 vs 45.7 GB). At 4 KB the run ended before freeing caught up.
4. **The off arm ran out of file descriptors** at 20:14 (999 open, soft limit 1,024): segment handles grew with the
   data (64 GB, 1,000 segments), and 19,892 PUTs failed with "too many open files". Issue #229, fixed in 4c2aeeb
   (handles capped at 4,096, soft limit raised to the hard limit at startup). The 256 MB arms were skipped: on these
   builds they would hit the same limit. Run 2 covers 256 MB on fixed builds with the limit raised.
