# RFC 6 - Compaction and Space Reclamation on the RFC 5 Engine

**Status:** Steps 1 (report only) and 2 implemented. Steps 3 to 5 to do.
**Builds on:** RFC 5 (one object service, single node as RS(1,0)), RFC 3 (segment files)
**Branch:** `feat-rfc6-compaction`

---

## Problem

Nothing frees disk space today.

| What | Grows forever because |
|---|---|
| Shard bytes | Each node appends every shard to one `.bin` file per bucket. DELETE and overwrite never remove a shard: `ShardMetaStore::delete` is never called. |
| Shards of failed writes | A PUT that misses quorum leaves its written shards behind (#206). |
| Old versions | An overwrite writes new shards under a new version key (#151). The old version's shards are never freed. |
| Record logs | `LocationStore`, `ContentLocationStore`, `ShardMetaStore` and `BucketConfigStore` are append-only logs. Each has a `compact()`, but nothing calls it, and it holds the write lock for the whole rewrite, which would stall every PUT. |
| Inline objects | Inline object bytes (RFC 5, T1) live in the location log, so they're only freed when that log is compacted. |

Measured on a 5-node cluster: a 20.8 MB PUT used 33.9 MB, an overwrite doubled that to 67.7 MB, and DELETE freed nothing.

RFC 3 designed segment files and a compactor for the single-node engine, with offsets in SQLite. RFC 5 moves every deployment, single node included, onto the cluster engine. This RFC adapts RFC 3 to that engine and adds what the cluster makes possible.

## Goals

1. **Space is reclaimed** for deletes, overwrites, failed writes and record logs.
2. **Compaction never blocks foreground I/O.** No lock is held across a copy. Foreground PUT and GET p99 stay within noise while compaction runs.
3. **DELETE is fast, reclamation is local.** DELETE removes the object's record on a quorum and returns. Each node frees the shard bytes on its own, later. Compacting a segment needs no network traffic.
4. **Durability is unchanged.** A byte is freed only once nothing can still reference it.
5. **Single node is not special.** RS(1,0) uses the same code.

## What the RFC 5 engine makes cheaper

Two facts about the RFC 5 engine make compaction cheaper than in RFC 3:

1. **Offsets are node-local.** A location record names the nodes that hold an object's shards (`shard_peers`), not where the bytes sit on disk. Where a shard sits is only in that node's `ShardMetaStore`. So moving a shard within a node changes one local index entry and nothing else in the cluster.
2. **Every node has a copy of the location records.** Records are sent to every node, so a node can tell locally whether a shard it holds still looks referenced:

   > A shard `(bucket, key, version, idx)` on node N is **referenced** if the location record for `(bucket, key)` has that `version` and `shard_peers[idx] == N`.

   Packed (content-dependent) shards use the same rule against `ContentLocationStore`.

So reclamation doesn't depend on a delete message reaching every shard holder. Lost deletes, failed writes and old versions all look the same, an unreferenced shard, and the node that holds it finds it.

**But a node's own copy can be stale.** Records reach a quorum of nodes, not all of them. So a node decides how sure it is by what it has:

| What the node has for the shard's `(bucket, key)` | Meaning | Action |
|---|---|---|
| A tombstone, or a record with a newer version | A delete or overwrite happened | Free on its own, after the grace period |
| A record with this version, naming this node | Referenced | Keep |
| No record at all | A failed PUT's shard, **or** this node missed a PUT's record | Ask every other node first. Free only if all say they don't reference it. |

- **Tombstone or newer version: no confirmation.** If another node missed that tombstone, it may still try to read the deleted object and fail. That's correct: the client deleted it.
- **No record: confirm first.** Here the node can't tell "never committed" from "I missed it", and the second case means freeing would drop a shard of a live object. The check is one batched request per peer, in the background. A peer that doesn't answer means "keep", and the candidate is retried on the next sweep. This case is rare: failed writes, and shards left over after a restart.

Moving bytes during compaction (below) needs no confirmation, because it frees nothing that's referenced.

**Grace period.** A shard is written before its record exists, because the record is replicated only after shard quorum (#151). So a shard is only treated as unreferenced once it's older than a grace period `G`, default 10 minutes, which covers any in-flight PUT. The shard index stores each shard's write time for this check.

## Design

### 1. Segment files (from RFC 3)

Each node stores shards in fixed-size segment files per bucket instead of one growing file:

```
storage/{user}/{bucket}/
  0000000000.seg   sealed
  0000000001.seg   sealed
  0000000002.seg   active: fresh writes
  c0000000000.seg  sealed survivor segment
  c0000000001.seg  active survivor segment: compaction writes
```

- `SEGMENT_SIZE` is 256 MB to start, so a compaction unit copies at most a few hundred MB.
- RFC 3's encoding stays: the shard index stores a global offset, with `segment = offset / SEGMENT_SIZE` and `local offset = offset % SEGMENT_SIZE`. Survivor segments use a separate id range.
- Deleting a bucket deletes its directory.

### 2. Shard index entries carry their identity

Today the shard index maps an opaque string key to `(offset, size)`. It becomes:

```
shard index entry: bucket, key, version, idx → segment offset, size, written_at
```

The liveness rule needs `(bucket, key, version, idx)`. Today these could only be recovered by parsing the key string from the right, so they're stored explicitly instead.

### 3. Live bytes per segment, in memory

Each node keeps `live_bytes[segment]` in memory. It's rebuilt from the shard index at startup, like the index itself, so it can't drift.

- **On a tombstone or a newer version:** when the node applies it, the old shards it holds are marked dead right away. It's an in-memory lookup, with no I/O and no network.
- **Orphan sweep:** every few minutes it walks the shard index and finds shards with no record older than the grace period, such as shards of failed writes, and confirms them with every peer in one batch.
- Dead shards are subtracted from `live_bytes`. Their index entries are removed when their segment is compacted or deleted.
- After a restart, tombstones already merged out of the log are gone, so their shards show up as "no record" and take the confirmed path. That's slower, but safe.

### 4. Two write heads: fresh and survivor

RFC 3 compared two options:
- **A:** copy survivors into the active segment. Lower space use, but compaction writes compete with fresh writes on the same file.
- **B:** reserve headroom in an older segment for survivors. No competition, but 50% more space at T = 0.5.

This RFC uses **C: a separate survivor segment stream.** Fresh writes go to the active segment. Compaction writes go to the active survivor segment, a different file.

| | Space amplification | Competes with fresh writes on the same file | Headroom reserved |
|---|---|---|---|
| A | 1/T | yes | no |
| B | (1+T)/T | no | yes |
| **C** | **1/T** | **no** | **no** |

C gets A's space cost with B's separation. It also separates by age. Survivors are, by definition, the objects that outlived a compaction, so they're likely long-lived. Survivor segments therefore stay full longer and are rarely compacted again. That lowers write amplification for long-lived data, the case where RFC 3's model shows it compounding (WA → 1/(1-T)). Device-level contention is handled by the rate limit below.

### 5. Picking a segment: cost-benefit, not a fixed threshold

RFC 3 compacts any segment below a cold threshold T. This RFC ranks sealed segments by the cost-benefit score from LFS (Rosenblum and Ousterhout, 1992):

```
score = (1 - u) × age / (1 + u)
```

where `u` is the segment's live fraction and `age` is the time since its newest write.
- **Benefit:** the space freed (1 − u), weighted by how unlikely the rest is to die soon (age).
- **Cost:** reading the segment and writing its survivors (1 + u).

It compacts the highest-scoring segment first, and only segments with `u` below a cap (default 0.5). A segment that's mostly dead but still being deleted from waits, because waiting is free and compacting it now would copy bytes that are about to die.

### 6. Copying survivors

For a chosen segment:

1. **Copy** each live shard into the active survivor segment with `copy_file_range`: an in-kernel copy with no userspace buffer. On XFS with reflink enabled (the `mkfs.xfs` default since xfsprogs 5.1), block-aligned ranges can be shared instead of copied. No lock is held.
2. **Sync** the survivor segment once for the whole batch.
3. **Commit** all new offsets to the shard index in one batched append, with one group-commit fsync. A shard deleted during the copy is skipped at this step: its index entry is gone, so its copied bytes are simply dead in the survivor segment.
4. **Unlink** the old segment after a grace period. A reader that looked up the old offset before the commit still has the file open, which Linux allows after unlink. A reader that gets `ENOENT` looks the shard up again and finds the new offset.

**Crash safety:**
- A crash before step 3: the old offsets are still valid and the old segment still exists. The partly written survivor bytes are unreferenced and get reclaimed later.
- A crash after step 3: the old segment is unreferenced, and is unlinked on restart.

**Block alignment.** Reflink needs offsets and lengths aligned to the filesystem block size (4 KiB). Aligning every shard costs padding. Small objects are inline (T1), so shards are at least a few KB, and the padding is up to 4 KiB per shard. Alignment becomes a benchmarked option: copy speed against padding cost.

### 7. Rate limit

- Compaction reads and writes go through a token bucket (default 50 MB/s per node, configurable).
- It pauses while the node's foreground request queue is deep, judged from recent request latency.
- At most `m` nodes compact at once, by a simple stagger on node index, so a read can always find `k` shards on nodes that aren't compacting. With hedged reads (RFC 5, T6), a read already avoids a slow node.

### 8. Record logs: merge without blocking

The four record logs keep their full state in an in-memory index, so the index is the source for compaction:

1. **Cut over** (holding the write lock for microseconds): new appends go to a new log file `log.N+1`.
2. **Snapshot**: walk the index in chunks of a few thousand entries, each under a brief read lock, writing them to `snapshot.N`, then fsync it. The snapshot doesn't need to be one point in time: any key that changes after the cutover is also in the new log, which replays after the snapshot and wins.
3. **Swap:** atomically rename the snapshot into place, then delete the logs up to `N`.

On restart, replay the snapshot, then every log after `N`. A key changed after the cutover appears in both, and the later log wins. A key deleted after the cutover has its tombstone in the newer log. Nothing holds the write lock while data is copied.

A log is merged when its size is more than twice the size of its live entries. Merging the location log also frees the bytes of deleted inline objects.

### 9. What a delete does

- **Coordinator:** replicates the record tombstone to every node and returns once a quorum acks, as today. It sends no shard deletes, so DELETE stays one round.
- **Each node, on applying the tombstone:** marks the shards it holds for that object dead. No confirmation is needed, since a tombstone is proof of a delete.
- **Nodes that missed the tombstone:** keep their shards until the tombstone reaches them, or until the orphan sweep confirms the shard is unreferenced everywhere.
- **Bytes on disk:** freed when the segment is compacted, or when the whole segment is dead, by unlinking it with no copy.

## Cost model

RFC 3's model carries over:
- space amplification `SA = 1/T` for options A and C
- write amplification `WA = 1 + k(T, L)`

What changes for option C: survivors go into their own segments, so a byte that survives one compaction lands among other long-lived bytes. Its segment is less likely to go cold again, which lowers `k` for long `L`. The benchmark measures `WA` directly.

## Alternatives considered

- **Hole punching** (`fallocate(PUNCH_HOLE)`, PR #202): frees space with no copy, but fragments files and depends on filesystem behavior. It's rejected as the main mechanism. It could later free large dead extents inside sealed segments.
- **Per-segment dead-byte counters on disk:** these drift after a crash. Rebuilding from the in-memory index at startup avoids drift.
- **Coordinator-sent shard deletes:** need reliable delivery to every shard holder, and a lost message leaks space forever. Local candidates plus a batched confirmation needs neither.
- **SQLite offsets (RFC 3 as written):** RFC 5 moves metadata to `LocationStore`. With SQLite as the optional single-node backend, only the shard index changes, so the same compactor applies.

## Benchmarks

On the RFC 5 GCP setup (5 × `n2-standard-2`, local NVMe), with logging at the default level:

| Measure | Workload | Target |
|---|---|---|
| Space reclaimed | Write 10 GB, delete or overwrite 70%, run compaction | Disk within 1/T of live data |
| Time to reclaim | Same | Report MB/s per node |
| Foreground impact (ρ) | 4 KB and 1 MB PUT/GET during compaction, against idle | p99 within noise |
| Write amplification | Steady churn with mixed object lifetimes | Report, compared with the RFC 3 model |
| Copy method | `copy_file_range` against read/write, aligned against unaligned | Pick the default |
| Record-log merge | 10M records, PUT p99 during a merge | No stall |

## Implementation steps

### Step 1 on the current layout

Since this RFC was written, S3 objects moved to blob addresses whose placement lives in the version and multipart-part rows (the S3 parity work), and every node holds a copy of those rows. The liveness rule is the same, applied to three kinds of shard:

| Shard | Referenced if | Unreferenced, past the grace period |
|---|---|---|
| S3 blob (internal bucket `\u{1}blobs\u{1}…`) | a live version or multipart-part row holds a blob with this `wire_key` | confirm with every peer |
| Location (plain key) | the key's live location record has this version | reclaimable if this node holds a newer record or tombstone, else confirm |
| Packed stripe (`\0cdstripe:…`) | the key's live content record has this version | as location |

- **No new shard index field.** A shard's `wire_key` ends in its version, `{nanos:x}-{seq:x}`, which is the write time. That is `written_at`.
- **Blob shards always confirm.** A node that missed a row can't tell it from a deleted one, so the tombstone shortcut applies only to location and packed shards.
- **Report only.** `POST /cluster/_internal/reclaim/report` and a background sweep (`WARPDRIVE_RECLAIM_INTERVAL_SECS`, default 300) report what could be reclaimed. Nothing is freed until the compactor (steps 3 and 4).

**Acceptance** (`acceptance/step1.sh`, local 3-node RS(2,1)): 20 objects, 5 overwrites, 4 deletes, one aborted and one completed 2-part multipart upload. Every node reports 11 reclaimable shards and 5,453,120 bytes, the independent count. A node restarted with its metadata log wiped keeps all 18 live shards (peers confirm them) and still finds the 11. With a peer down, nothing is reported reclaimable.

### Step 2: record logs compacted without stalling writers

`BitcaskEngine::compact` no longer holds the write lock for the rewrite. A single log file keeps the design simple:

1. **Cut**, under the write lock for an instant: note the log's length. Every line before it is in the index.
2. **Snapshot**, without the write lock: the live entries, copied out of the index 4,096 at a time under brief read locks, to a temp file, synced.
3. **Tail and swap**, under the write lock: append the lines written since the cut, sync, rename over the log.

Replaying the result gives the index: a key changed after the cut is in the tail and wins. A writer waiting on its group-commit sync at the swap is covered, because its line is in the temp file and the temp file is synced before the rename.

A log is due when it has at least 10,000 lines and more than twice as many lines as live entries. A maintenance thread checks every 60 s and compacts the S3 metadata rows, the shard index, and the location and content-location logs when due. `POST /cluster/_internal/compact_logs` runs the check now.

**Acceptance:** crate tests run 4 writers putting and deleting during a compaction of a 40,000-line log: the slowest write took under half the compaction's time, and the compacted log replays to the final index. On a local 3-node cluster (`acceptance/step2.sh`), 15,000 overwrites of 50 keys left the metadata log at about 2,800 lines: the maintenance thread had compacted it while the writes ran. After a restart, all 50 keys hold their latest version.

### Segments or one file per bucket

Today each node keeps one append-only file per bucket. Two one-file designs were run against segments on the same 3 VMs (`n2`, local NVMe, XFS with reflink), with `bench/configs-onefile.txt` and `bench/configs-small.txt`. Medians, physical SA with the range across VMs:

| Design | 64 KiB to 4 MiB objects: SA | WA | MB/s | 2 KiB objects: SA | WA | MB/s |
|---|---|---|---|---|---|---|
| No compaction | 5.73 [5.69-5.83] | 1.00 | 325 | 5.74 [5.34-5.92] | 1.00 | 22.2 |
| One file, whole-file rewrite | 4.88 [3.59-4.92] | 1.32 | 153 | | | |
| One file, hole punching | **1.01** [1.01-1.01] | 1.00 | 320 | 4.67 [4.66-4.71] | 1.00 | 22.2 |
| Segments 64 MB | 3.13 [3.12-3.15] | 1.15 | 322 | **2.80** [2.76-2.91] | 1.88 | 20.7 |
| Segments 256 MB | 2.98 [2.95-3.06] | 1.15 | 323 | | | |

- **Whole-file rewrite is out:** it halves foreground throughput and still uses 4 to 5 times the disk.
- **Hole punching is best for large objects** (disk equal to live data, no copying) and **nearly useless for small ones**: a 2 KiB shard shares its 4 KiB block with neighbours, so a block is freed only when all of them die.
- **Segments are best for small objects**, the 4 KB workload that dominates, at the cost of copying (WA 1.88) and about 7% foreground throughput.
- Not measured: fragmentation from punching over hours (runs are 60 s for large objects, 290 s for small).

So neither alone covers both sizes. The data points to segments with the compactor (steps 3 and 4), plus hole punching inside sealed segments for dead ranges of whole blocks, which frees large objects at once without a copy. That was this RFC's original hint; the large-object numbers now say it's worth doing.

### When to compact: what the matrix measured

Medians over 3 VMs (`n2-standard-2`, local NVMe, XFS unless noted) from the segment benchmark:

| Setting | Physical SA | WA | Write p99 |
|---|---|---|---|
| No compaction | 5.55 | 1.00 | 88 ms |
| Baseline (256 MB, `umax` 0.5, 50 MB/s, C, `copy_file_range`) | 2.34 | 1.40 | 112 ms |
| `umax` 0.7 | 2.07 | 1.40 | 112 ms |
| `umax` 0.2 | 2.60 | 1.19 | 97 ms |
| 25 MB/s | 2.87 | 1.19 | 110 ms |
| 100 MB/s | 2.11 | 1.76 | 112 ms |
| 64 MB segments | 2.20 | 1.41 | 114 ms |
| read/write copy | 2.76 | 1.36 | 178 ms |

Each node compacts on its own, continuously in the background: a sealed segment once its live fraction is under `umax` (0.6 to 0.7 frees more at no extra write cost), at 50 MB/s or more (25 MB/s falls behind), with 64 to 128 MB segments and `copy_file_range`. Step 4 starts from these and is validated against them.

Each step is a PR that keeps the Ceph enabled list green and the RFC 5 performance gates.

| Step | Change | Done when |
|---|---|---|
| 1 | Shard index stores `(bucket, key, version, idx, written_at)`. `live_bytes` per file, the orphan sweep and peer confirmation, in report-only mode (a metric for reclaimable bytes). | Reclaimable bytes match an independent count, and a node that missed records never nominates a referenced shard |
| 2 | Record-log merge without blocking, run on the size trigger | 10M-record merge with no PUT stall |
| 3 | Segment files on the `Storage` trait (RFC 5, step 1) | Ceph green, no throughput regression |
| 4 | Compactor: two write heads, cost-benefit ranking, rate limit, unlink grace | Space-reclaimed and foreground-impact benchmarks pass |
| 5 | `copy_file_range` and the alignment option, benchmarked | Default chosen from data |

Step 1 already makes the problem visible: each node reports how much space it could reclaim.

## Open questions

1. **Confirmation when a node is down.** Only "no record" shards need every node to confirm, so a long outage pauses only those (failed writes, post-restart leftovers). Deletes and overwrites still reclaim. Is that acceptable?
2. **Grace period `G`.** 10 minutes covers any PUT today. Should it come from the PUT timeout instead?
3. **Segment size.** 256 MB keeps a compaction unit small. Is it too many files for buckets with heavy churn?
4. **Survivor generations.** Should survivors that survive again go into a third stream (LFS-style multiple generations), or are two enough?
5. **Rate limit default.** 50 MB/s per node is a guess until the foreground-impact benchmark runs.
