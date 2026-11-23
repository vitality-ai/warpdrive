# Soak 1: one hour on #224 at 4c2aeeb

Question: does #224 (segments, gc/ rows, chunked scans, capped segment handles) hold up for an hour of steady load:
flat memory and logs, disk near live data, no errors, every byte intact?

RS(2,1) on the 3 nodes, 64 MB segments, grace 120 s, servers started with `ulimit -n 1048576`. Background: `s3_bench`
overwriting 100,000 keys of 4 KB at 64 clients for 30 min, then reading for 30 min. Alongside for the whole hour,
`scripts/canary.py` (8 threads) keeps its own objects with unique bytes (1 KB to 2 MB, and 10-15 MB multipart), checks
every GET and LIST against what it wrote, deletes, completes and aborts multipart uploads, then reads every object back.
Samples every 5 s per node (`scripts/soak_sampler.sh`). Driver `scripts/soak.sh`.

## Correctness: held

- Canary: 1,639 objects at the end, **0 mismatches** on the final read-back, **0 failures** in the live checks.
- Background load: 3.01 M overwrites at 1,673/s and 27.0 M reads at 15,024/s, **0 errors**.
- Final audit sweep: 101,856 shards on node 0, all referenced, 0 reclaimable: the gc/ rows accounted for every dead
  shard.

## Resources: failed

| Per node | Start | End of writes (30 min) | Through the reads |
|---|---|---|---|
| Memory (RSS) | 0.1-0.2 GB | 6.9-7.3 GB | flat, never released |
| Metadata logs | 0 | 3.6 GB | flat |
| CPU | 55-88% | 60-88% | node 0 95% with no writes |
| Segment data | 0.1 GB | 10-14 GB | 1.5 and 2.4 GB on nodes 0 and 1 at the end; node 2 10.4 GB |
| Open descriptors | 100-270 | 300-520 | 200-330 (the 4,096 cap holds) |

**The kernel killed WarpDrive for running out of memory**: node 2 at minute 57 (7.7 GB resident on a 7.9 GB VM) and
node 1 at 01:30, just after the run (`oom-kills.txt`). Reads went on through node 2's loss (RS(2,1) tolerates one), so
the canary still passed.

## Cause and fix

gc/ rows were kept for a day after processing (`WARPDRIVE_GC_RETAIN_SECS` 86,400), so at about 2,800 overwrites a
second millions of them sat in the in-memory index and the metadata log; and each pass scanned every gc/ row to find
the few after its cursor, so a pass cost grew with them, CPU pinned and freeing fell behind. Fixed in 1f8bb42: a pass
reads only the range after its cursor up to now minus the grace period, and drops processed rows from the node's copy
at once. Soak 2 runs the same hour on 1f8bb42.
