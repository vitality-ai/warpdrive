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
    0000000001.seg    ← sealed, has headroom reserved for compaction writes
    0000000002.seg    ← active, append-only (fresh writes only)
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

Example: object at local offset 200 MB inside segment 1 → global offset `736,870,912`. Decode: `736,870,912 / 512MB = 1` → open `0000000001.seg`, seek to `200,000,000`.

**No schema change.** The existing `offset_size_list` blob stores `(global_offset, size)` pairs unchanged.

### Write Path

A segment is sealed when it reaches `SEGMENT_SIZE - HEADROOM` bytes. The remaining `HEADROOM` is reserved for compaction writes only.

1. If `active_size + write_size ≤ SEGMENT_SIZE - HEADROOM`: append to active segment
2. Otherwise: seal active segment, open next segment, write there

Large writes that overflow are split into multiple extents (one per segment). The existing `offset_size_list` supports multiple extents per object — no schema change needed.

### Read Path

For each extent: decode `segment_id = offset / SEGMENT_SIZE`, `local_offset = offset % SEGMENT_SIZE`, open the file, seek, read. Concatenate across extents.

### Bucket Deletion

Delete the entire `storage/{bucket}/` directory. No per-object scan needed.

---

## Space Reclamation via Compaction

### Utilization — Computed On Demand

For segment N, live bytes come directly from the `objects` table. What is absent from the table is dead.

```
live_bytes(N) = SUM of bytes from extents in objects
                that overlap [N*SEGMENT_SIZE, (N+1)*SEGMENT_SIZE)
```

No counter maintained. No drift on crash.

### Headroom and Compaction Target

Each sealed segment has `HEADROOM = f * SEGMENT_SIZE` bytes reserved. When compacting a cold segment, survivors are written into the headroom of `active - 1` rather than the active segment. This keeps fresh writes and compaction writes in separate segments.

```
Segment N-2:  [==live==|--dead--|  headroom  ]  ← cold target
Segment N-1:  [==live==|--dead--|  headroom  ]  ← compaction destination
Segment N:    [===fresh writes...            ]  ← active, untouched by compaction
```

### Compaction Process

1. Compute utilization for all sealed segments except `active - 1`
2. Pick the lowest-utilization segment below `COLD_THRESHOLD T`
3. Copy its live bytes into the headroom of `active - 1`
4. Update `offset_size_list` in `objects` within a single SQLite transaction
5. After commit: delete the cold `.seg` file

Crash safe: old file deleted only after commit. Readers on the old file before commit continue reading valid data. No locking needed.

---

## Optimization: Finding Optimal Parameters

The analytical tables common in storage RFCs just restate what we already know. Instead, we formulate an optimization to find the best values of `T` (cold threshold) and `f` (headroom fraction) given how much we care about write amplification vs. space amplification.

### Variables and Expressions

Let:
- `T` ∈ (0, 0.5] — cold threshold (compact segments below this utilization)
- `f` ∈ [0, 0.5] — headroom fraction of SEGMENT_SIZE
- `u` — utilization at compaction time; worst case `u = T`

Setting `u = T` (worst case), the key metrics are:

```
WA = 1 + T                          (write once fresh, copy T fraction during compaction)
SA = 1 / ((1 - f) * T)              (live fraction of data capacity at threshold)
```

Headroom feasibility — the headroom in `active - 1` must fit the survivors from one cold segment:

```
f * S ≥ T * (1 - f) * S
f ≥ T / (1 + T)                     (minimum headroom to absorb survivors)
```

Setting `f = T / (1 + T)` (minimum feasible headroom) and substituting into SA:

```
SA = (1 + T) / T
```

### Optimization Problem

Minimize a weighted combination of WA and SA:

```
minimize    α * WA + β * SA
            = α * (1 + T) + β * (1 + T) / T
            = (1 + T)(α + β / T)

subject to  0 < T ≤ 0.5
            α + β = 1,  α ≥ 0,  β ≥ 0
```

This is a single-variable unconstrained minimization in `T`. Taking the derivative and setting to zero:

```
d/dT [(1 + T)(α + β/T)]  =  α + β(T - (1+T)) / T²
                          =  α - β / T²  =  0

T* = sqrt(β / α)
f* = T* / (1 + T*)
```

### Closed-Form Solution

| Priority | α | β | T* | f* | WA | SA |
|---|---|---|---|---|---|---|
| Only WA | 1.0 | 0.0 | → 0 | → 0 | → 1.0 | → ∞ |
| WA >> SA | 0.8 | 0.2 | 0.50 | 0.33 | 1.50 | 3.0 |
| Balanced | 0.5 | 0.5 | 0.50 | 0.33 | 1.50 | 3.0 |
| SA >> WA | 0.2 | 0.8 | 0.50 | 0.33 | 1.50 | 3.0 |
| Only SA | 0.0 | 1.0 | → ∞ | → 1 | → ∞ | → 1.0 |

`T*` is capped at 0.5 for any β/α ≥ 1. The solution is insensitive to the α/β ratio across a wide range — `T = 0.5`, `f = 0.33` (33% headroom) is optimal for almost all practical weightings. The pure extremes (only WA or only SA) are degenerate: one gives infinite space usage, the other infinite write work.

This gives us the proposed constants:
- `COLD_THRESHOLD = 0.30` (slightly conservative vs. T*=0.5, to avoid compacting segments that are temporarily cold)
- `HEADROOM = 150 MB` on a 512 MB segment = 29% ≈ f*=0.33

### Benchmark Plan

The model will be validated against measured results. For each approach (baseline, segments-B without headroom, segments-C with headroom):

1. **Write throughput** — sequential write of 10 GB at object sizes 1 KB / 1 MB / 100 MB. Measure MB/s.
2. **Read latency** — random reads after compaction. Measure p50 / p99 latency and MB/s.
3. **Space reclaimed** — write 10 GB, delete 70%, run one compaction cycle. Measure bytes on disk before/after and wall time.
4. **Measured WA** — instrument bytes written to storage during workload + compaction. Compare against model prediction `1 + T`.
5. **Write interference (B only)** — concurrent fresh writes during compaction. Measure throughput drop.

Measured WA and SA will be plotted against the model predictions. If they diverge, the model assumptions (particularly `u = T` worst case) will be revised.

---

## Migration from Current Layout

Current layout: one flat file per bucket at `storage/{bucket}`.

1. Detect if `storage/{bucket}` is a plain file
2. Rename to `storage/{bucket}_migrate`
3. Create `storage/{bucket}/` directory
4. Move to `storage/{bucket}/0000000000.seg`
5. Set active segment to `0000000001.seg`

Existing offsets decode to `segment_id = 0` for files under 512 MB. Files exceeding 512 MB require a split migration with offset rewrite (separate step).

---

## Constants (Proposed)

| Constant | Value | Derivation |
|---|---|---|
| `SEGMENT_SIZE` | 512 MB | Design choice: amortizes overhead, compacts in seconds |
| `HEADROOM` | 150 MB (29%) | Near f* = 33% from optimization |
| `COLD_THRESHOLD` | 30% | Slightly below T* = 50%; conservative to avoid false-cold |
| `MIN_AGE` | 1 hour | Avoid compacting recently-sealed segments |
| `COMPACTION_BATCH` | 1 segment per GC cycle | Bounds GC cycle duration |

---

## Non-Goals

- In-place defragmentation within a segment
- Cross-bucket compaction
- Replication or erasure coding (separate RFC)
