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

Segment identity is encoded directly into the global offset stored in SQLite:

```
global_offset  = segment_id * SEGMENT_SIZE + local_offset
segment_id     = global_offset / SEGMENT_SIZE
local_offset   = global_offset % SEGMENT_SIZE
```

**No schema change required.** The existing `offset_size_list` blob in the `objects` table continues to store `(offset, size)` pairs. The storage layer decodes which segment file and local seek position to use at read time.

### Write Path

1. Append bytes to the active segment
2. If the write would overflow the segment (i.e. `current_size + write_size > SEGMENT_SIZE`), seal the current segment and open a new one
3. An object extent is always contained within a single segment — no extent crosses a boundary
4. Return `global_offset = segment_id * SEGMENT_SIZE + local_offset`

### Read Path

1. Decode `segment_id = offset / SEGMENT_SIZE`, `local_offset = offset % SEGMENT_SIZE`
2. Open `storage/{bucket}/{segment_id:010}.seg`
3. Seek to `local_offset`, read `size` bytes

### Bucket Deletion

Delete the entire `storage/{bucket}/` directory. No per-object scan needed.

---

## Space Reclamation via Compaction

### Segment Utilization

For any sealed segment, live utilization is computed from SQLite:

```sql
SELECT SUM(size) FROM objects
WHERE <extent falls within segment N>
```

Since extents are stored as blobs, a simpler approach tracks utilization in a `segments` table (see below).

### Segment Registry (New SQLite Table)

```sql
CREATE TABLE segments (
    bucket        TEXT NOT NULL,
    segment_id    INTEGER NOT NULL,
    total_bytes   INTEGER NOT NULL DEFAULT 0,
    live_bytes    INTEGER NOT NULL DEFAULT 0,
    sealed        BOOLEAN NOT NULL DEFAULT FALSE,
    created_at    TEXT NOT NULL,
    PRIMARY KEY (bucket, segment_id)
);
```

- `total_bytes`: how much data was ever written to this segment
- `live_bytes`: updated on every PUT (increment) and every deletion event (decrement)
- `utilization = live_bytes / total_bytes`

### Hot / Cold Cost Model

A segment is a compaction candidate when:

```
utilization < COLD_THRESHOLD   (e.g. 30%)
AND sealed = TRUE
AND age > MIN_AGE              (e.g. 1 hour — avoid compacting segments with recent writes)
```

Priority score (lower = compact first):

```
score = utilization * (1 + recency_factor)
recency_factor = hours_since_last_write / 24
```

This means old, mostly-empty segments are compacted first. A segment that was recently active but mostly deleted still scores higher than an ancient near-empty one.

### Compaction Process

Run by the existing deletion worker background task:

1. Query `segments` for the lowest-scoring candidate below `COLD_THRESHOLD`
2. Read all live objects from that segment (look up extents via SQLite for all objects whose offsets fall in that segment)
3. For each live object extent:
   - Append bytes to the active segment (new global offset)
   - Update the object's `offset_size_list` in `objects` table
   - Update `live_bytes` on old and new segments
4. Wrap steps 2–3 in a SQLite transaction — readers are unaffected until the commit
5. After commit: delete the `.seg` file
6. Remove the segment row from `segments`

If the process is interrupted mid-compaction, the old `.seg` file still exists and the old offsets are still valid in SQLite (transaction was not committed). Safe to retry.

---

## Migration from Current Layout

Current layout: one file per bucket at `storage/{bucket}` (a flat file, no extension).

Migration on first startup:
1. Detect if `storage/{bucket}` is a plain file (not a directory)
2. Rename it to `storage/{bucket}_migrate_tmp`
3. Create `storage/{bucket}/` directory
4. Move `storage/{bucket}_migrate_tmp` → `storage/{bucket}/0000000000.seg`
5. Insert a row in `segments` with `total_bytes = file_size`, `live_bytes` computed from SQLite, `sealed = TRUE`
6. Active segment becomes `0000000001.seg`

Existing offsets in SQLite are unchanged — they decode into `segment_id=0` (since all offsets < 512 MB decode to segment 0) assuming the current file is under 512 MB. If the current file exceeds 512 MB, migration must split it into multiple numbered segments and rewrite offsets — a heavier migration handled separately.

---

## Constants (Proposed)

| Constant | Value | Rationale |
|---|---|---|
| `SEGMENT_SIZE` | 512 MB | Large enough to amortize metadata overhead, small enough to compact quickly |
| `COLD_THRESHOLD` | 30% | Compact when less than 30% of a segment is live |
| `MIN_AGE` | 1 hour | Avoid compacting segments written to very recently |
| `COMPACTION_BATCH` | 1 segment per GC cycle | Keeps GC cycle bounded; GC runs every 5 min |

---

## Open Questions

1. **Concurrent reads during compaction**: Reads on the old segment file are safe since we don't delete the file until after the SQLite commit. But should we hold a read lock on the segment file during compaction to avoid a race where a reader opens the file just as it's deleted?

2. **Segment size vs object size**: Objects larger than `SEGMENT_SIZE` (e.g. a 1 GB multipart upload part) cannot fit in one segment. Options: (a) raise `SEGMENT_SIZE`, (b) allow multi-segment extents (breaks the no-schema-change guarantee), (c) cap individual part writes at `SEGMENT_SIZE`. Needs resolution before implementation.

3. **live_bytes accounting accuracy**: If the server crashes between writing an object and updating `live_bytes` in `segments`, the counter drifts. A periodic reconciliation job (recompute `live_bytes` from `objects` table) would keep it accurate.

4. **`segments` table bootstrap**: On first startup with an existing database, `live_bytes` for segment 0 must be computed from the `objects` table rather than tracked incrementally.

---

## Non-Goals

- In-place defragmentation within a segment
- Cross-bucket compaction
- Replication or erasure coding (separate RFC)
