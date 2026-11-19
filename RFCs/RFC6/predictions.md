# RFC 6 experiment: predictions

Written before any matrix result was read, while the runs were in progress. The results are judged against these.

**Setup:**
- **Hardware:** `n2-standard-2`, local NVMe, XFS with reflink.
- **Live set:** 6 GiB, pre-filled at steady state.
- **Churn:** 18 GiB measured, after 6 GiB of warm-up.
- **Objects:** 60% 64 KiB, 30% 1 MiB, 10% 4 MiB.
- **Lifetimes:** 90% of new objects are hot (mean life 0.24× the live set, in bytes written). The rest are cold (7.9×). Cold objects are about 79% of live bytes.
- **Load:** 8 writers and 4 readers at full speed.
- **Baseline:** 256 MB segments, compact when a segment is under 50% live (`umax` 0.5), cost-benefit ranking, separate survivor stream (C), `copy_file_range`, 50 MB/s rate limit.

**Metrics:**

| Metric | Meaning |
|---|---|
| SA | Disk bytes ÷ live bytes. Physical: as the filesystem reports it. Logical: sum of segment file sizes. |
| WA | (Foreground + compaction bytes written) ÷ foreground bytes |
| rho | Foreground MB/s with no compaction ÷ foreground MB/s in this run |

**Rules for a real effect:** all three VMs agree on its sign, and it's bigger than the baseline's spread across VMs (and the within-VM repeat spread, once measured).

| Factor | Prediction | Reason |
|---|---|---|
| `umax` (0.2 → 0.7) | SA falls, WA rises | RFC 3: SA ≈ 1/T. Each compaction copies a u fraction of the segment, so WA ≈ 1 + k(T, L) and rises with T. Above about 0.5, WA grows faster than SA falls. |
| `umax` and rho | rho rises with `umax` until the rate limit binds, then flattens | More compaction work. The 50 MB/s limit caps it. |
| Segment size (32 → 1024 MB) | SA rises with size. WA roughly flat. File count falls in proportion. | A segment mixes objects of different ages. Bigger segments take longer to fill and fall below `umax` later, so dead bytes sit longer. Small segments isolate deaths better. WA depends on u at compaction time, not on size. |
| Segment size, smallest values | Small segments are no worse than 256 MB on rho or read p99 | Copy batches are smaller, but the rate limit dominates. Only file count changes visibly. |
| Victim choice: greedy vs cost-benefit | Cost-benefit: lower WA, similar or slightly higher SA | LFS: cost-benefit avoids compacting hot segments that are still dying, which greedy picks first. With 90% hot objects, this should be visible. |
| Survivor placement: A vs C | C: lower WA over the run and lower rho. SA similar. | C groups survivors (likely long-lived) into their own segments, which then stay full. A mixes them back with hot data. rho falls because the active file isn't shared. On one disk, part of the interference is the device, not the file, so the rho difference may be small. |
| Copy method on XFS: `copy_file_range` vs read/write | `copy_file_range`: lower rho, same WA | Offsets are 64 KiB-aligned, so XFS can share blocks instead of copying. Compaction then costs far less device bandwidth. Logical WA counts the same bytes either way. |
| Copy method on ext4 | Gap much smaller than on XFS | ext4 copies in the kernel with no block sharing, so it saves only the user-space round trip. |
| Rate limit (25 → unlimited) | SA falls as the rate rises. rho rises. Unlimited has the highest rho. At 25 MB/s, compaction may not keep up, and SA keeps growing during the run. | Compaction throughput has to cover the dead-byte production rate. |
| No compaction | SA grows during the whole run, WA = 1, rho = 1 by definition | Nothing is reclaimed except fully dead segments, which are rare with mixed lifetimes. |
| Physical vs logical SA (XFS) | Physical is higher by a roughly constant amount | XFS speculative preallocation on growing files (seen in the smoke run: 2.3 vs 1.7). |

**What the ILP is expected to pick** (balanced weights): `umax` 0.4–0.5, segment size at the smallest value that keeps the file count reasonable (64–128 MB), cost-benefit, C, `copy_file_range`, and the lowest rate limit that keeps SA from growing.
