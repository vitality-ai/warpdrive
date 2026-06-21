# RFC 3 — Segment-Based Storage with Space Reclamation

**Status:** Draft  
**Branch:** `feat/rfc-segment-storage`

---

## Problem

The current storage layer is a single append-only file per user/bucket. Object location is stored as `(offset, size)` extents in SQLite. Deletes queue the extents via the deletion WAL but the background worker has no way to actually reclaim space — the file grows forever with holes.

There is no compaction, no merging, and no way to shrink disk usage after objects are deleted or overwritten.

---

## Goals

- **Writes stay append-only** — no seek-before-write, no in-place mutation
- **Reads stay O(1)** — direct seek by offset within a segment file
- **Space is actually reclaimed** — deleted object extents are freed by removing whole segment files
- **No metadata schema change** — the existing `(offset, size)` extent format is preserved; segment identity is encoded into the global offset
- **No new SQLite tables** — segment list comes from the filesystem, utilization computed from the existing `objects` table

---

## Design

### Segment Files

Storage is divided into fixed-size segment files per bucket:

```
storage/
  {bucket}/
    0000000000.seg    ← sealed, read-only
    0000000001.seg    ← sealed, read-only
    0000000002.seg    ← active, append-only
```

Each segment has a fixed maximum size `SEGMENT_SIZE` (proposed: 512 MB). The active segment is the highest-numbered one. Only one segment is written to at a time.

### Encoding: Global Offset

Segment identity is encoded directly into the global offset stored in SQLite. With `SEGMENT_SIZE = 512 MB = 536,870,912 bytes`, the virtual address space is:

```
Segment 0: bytes           0  →  536,870,911   → 0000000000.seg
Segment 1: bytes 536,870,912  → 1,073,741,823  → 0000000001.seg
Segment 2: bytes 1,073,741,824 → ...           → 0000000002.seg
```

At read time, one number gives you everything:

```
segment_id   = global_offset / SEGMENT_SIZE   → which file to open
local_offset = global_offset % SEGMENT_SIZE   → where to seek inside it
```

Example: object written at local offset 200 MB inside segment 1 is stored as global offset `736,870,912`. At read time: `736,870,912 / 512MB = 1` → open `0000000001.seg`, seek to `736,870,912 % 512MB = 200,000,000`.

**No schema change required.** The existing `offset_size_list` blob in the `objects` table stores `(global_offset, size)` pairs unchanged.

### Write Path

1. Check if `current_active_size + write_size <= SEGMENT_SIZE`
2. If it fits: append to active segment, return `global_offset = segment_id * SEGMENT_SIZE + local_offset`
3. If it overflows: split the write across the current segment and a new one, returning two extents (see below)

### Large Objects — Extent Splitting

Objects larger than `SEGMENT_SIZE`, or writes that overflow the active segment, are split into multiple extents — one per segment. The existing `offset_size_list` already supports multiple extents per object, so no schema change is needed.

Example: writing a 600 MB object when the active segment has 100 MB remaining:

```
extent 1 → segment N,   local_offset = 412 MB, size = 100 MB
extent 2 → segment N+1, local_offset = 0,      size = 500 MB
```

SQLite stores `[(N*512MB + 412MB, 100MB), ((N+1)*512MB, 500MB)]`. At read time, each extent is decoded independently and the bytes are concatenated. Each extent is always contained within a single segment file.

### Read Path

For each extent in `offset_size_list`:
1. Decode `segment_id = offset / SEGMENT_SIZE`, `local_offset = offset % SEGMENT_SIZE`
2. Open `storage/{bucket}/{segment_id:010}.seg`
3. Seek to `local_offset`, read `size` bytes
4. Concatenate across extents

### Bucket Deletion

Delete the entire `storage/{bucket}/` directory. No per-object scan needed.

---

## Space Reclamation via Compaction

### Segment Utilization — Computed On Demand

No counter is maintained. At compaction-candidate selection time, live utilization for segment N is computed directly from the `objects` table:

```sql
SELECT COALESCE(SUM(
    MIN(extent_end, segment_end) - MAX(extent_start, segment_start)
), 0)
FROM <extents derived from offset_size_list>
WHERE extent_start < segment_end AND extent_end > segment_start
```

Where `segment_start = N * SEGMENT_SIZE` and `segment_end = (N+1) * SEGMENT_SIZE`.

This avoids any counter drift on crash. No reconciliation job needed.

### Segment List

Derived from the filesystem at compaction time: `ls storage/{bucket}/*.seg` sorted numerically. The highest-numbered file is the active segment (excluded from compaction).

### Hot / Cold Cost Model

A segment is a compaction candidate when:

```
sealed = TRUE  (not the active segment)
AND utilization < COLD_THRESHOLD  (e.g. 30%)
AND age > MIN_AGE  (e.g. 1 hour)
```

Priority score (lower = compact first):

```
score = utilization * (1 + age_hours / 24)
```

Old, mostly-empty segments are compacted first. Age is derived from the segment filename (segment_id encodes creation order, mtime from the filesystem).

### Compaction Process

Run by the existing deletion worker background task:

1. List sealed segments for each bucket, compute utilization
2. Pick the lowest-scoring candidate below `COLD_THRESHOLD`
3. For each live object whose extents overlap that segment:
   - Read the bytes from the old segment
   - Append to the active segment (get new global offsets)
   - Update `offset_size_list` in the `objects` table
4. Wrap all SQLite updates in a single transaction
5. After commit: delete the `.seg` file

**Crash safety**: the old `.seg` file is only deleted after the SQLite transaction commits. If the process crashes mid-compaction, the old offsets are still valid and the old file still exists. Safe to retry on next GC cycle.

**Read safety during compaction**: readers that opened the old segment file before the commit continue reading valid data — the file is not deleted until after the commit. Readers arriving after the commit see the new offsets. No locking needed.

---

## Migration from Current Layout

Current layout: one flat file per bucket at `storage/{bucket}` (no extension, no directory).

Migration on first startup:
1. Detect if `storage/{bucket}` is a plain file (not a directory)
2. Rename to `storage/{bucket}_migrate`
3. Create `storage/{bucket}/` directory
4. Move `storage/{bucket}_migrate` → `storage/{bucket}/0000000000.seg`
5. Set active segment to `0000000001.seg` (empty, ready for new writes)

Existing offsets in SQLite decode to `segment_id = 0` as long as the migrated file is under 512 MB. If the file exceeds 512 MB, it must be split at 512 MB boundaries with offsets rewritten — handled as a separate migration step gated on file size.

---

## Constants (Proposed)

| Constant | Value | Rationale |
|---|---|---|
| `SEGMENT_SIZE` | 512 MB | Large enough to amortize overhead; small enough to compact in seconds |
| `COLD_THRESHOLD` | 30% | Compact when less than 30% of a segment is live |
| `MIN_AGE` | 1 hour | Avoid compacting recently-sealed segments |
| `COMPACTION_BATCH` | 1 segment per GC cycle | Keeps each GC cycle bounded; GC runs every 5 min |

---

## Non-Goals

- In-place defragmentation within a segment
- Cross-bucket compaction
- Replication or erasure coding (separate RFC)
