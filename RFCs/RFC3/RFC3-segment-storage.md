# RFC 3 — Segment-Based Storage with Space Reclamation

**Status:** Draft  
**Branch:** `feat/rfc-segment-storage`

---

## Problem

The current storage layer is a single append-only file per user/bucket. Object location is stored as `(offset, size)` extents in SQLite. Deletes queue the extents via the deletion WAL but the background worker has no way to actually reclaim space — the file grows forever with holes.

There is no compaction, no merging, and no way to shrink disk usage after objects are deleted or overwritten.

---

## Design Journey

This section records how each design decision was reached — what we observed, what we tried, why we changed direction. The goal is to make the final design legible: not just what it is, but why it isn't something simpler.

---

### Step 1 — Observing the problem: the file grows forever

The current layout is one flat append-only file per bucket at `storage/{bucket}`. Object metadata (offset, size) lives in SQLite. When an object is deleted, we remove the SQLite row and queue the extent in a deletion WAL. A background worker reads the WAL — but all it can do is note that those bytes are now dead. There is no mechanism to return them to the OS. The file only grows.

After any workload with significant churn (overwrites, lifecycle deletes), disk usage diverges from live data size. There is no compaction, no hole-punching, no reclaim path.

**Observation**: we need a storage layout where "freeing space" means deleting an entire file, not punching holes in a single large one.

---

### Step 2 — First attempt: compact within the flat file

The naive fix is to rewrite the flat file in place: scan for holes, copy live data forward, shrink the file. This breaks immediately:

- Every copy changes an offset. That means a SQLite UPDATE for every live object in the file — potentially millions of rows in one transaction.
- An incomplete compaction (crash mid-copy) leaves the file in an inconsistent state with no safe recovery path.
- Reads are blocked for the entire duration.

**Conclusion**: in-place compaction of a flat file is not viable. We need a layout where space can be reclaimed by deleting a self-contained unit without touching any other data.

---

### Step 3 — Segments: reclaim a whole file at a time

Splitting the bucket storage into fixed-size segment files gives us that unit. A cold, under-utilized segment can be compacted by copying its survivors elsewhere and then deleting the entire `.seg` file. No other segment is touched.

The immediate concern: how do we encode segment identity in the metadata? Adding a `segment_id` column to SQLite is a schema migration across all existing data. We wanted to avoid that.

**Key insight**: the global offset already encodes segment identity via integer division. With `SEGMENT_SIZE = 512 MB`:

```
segment_id   = global_offset / SEGMENT_SIZE
local_offset = global_offset % SEGMENT_SIZE
```

The existing `(offset, size)` pairs in SQLite are unchanged. The reader just needs to compute two extra integers before opening the file. No migration, no schema change.

![Virtual address space to segment file decoding](plot_address_space.png)

---

### Step 4 — Utilization: how do we know when a segment is cold?

The first instinct was a separate per-segment counter table — increment on write, decrement on delete. This introduces two failure modes: counter drift on crash, and a new table that has to be kept in sync with the main `objects` table.

Then we noticed: the information already exists. A row in `objects` means the extent is live. No row means it is dead. To compute utilization for segment N, query the `objects` table for all extents that overlap segment N's byte range and sum them. No counter, no drift, no new table.

```sql
SELECT SUM(extent_size) FROM objects
WHERE extent_start >= N * SEGMENT_SIZE
  AND extent_start  < (N+1) * SEGMENT_SIZE
```

![Space amplification over time across write, delete, and compaction phases](plot_utilization_timeline.png)

---

### Step 5 — Where do survivors go?

When we compact a cold segment, the live bytes have to go somewhere. Two options:

**Option B** — compact into the active segment. Simple: survivors are just appended like any other write. No structural change needed.

**Option C** — reserve headroom in `active - 1`, compact there. This completely separates fresh writes (active) from compaction writes (active - 1). The active segment is never touched by the compactor.

The motivation for C is architectural clarity: the active segment has a single writer and a single write pattern (append-only, sequential). Adding compaction writes would mean two concurrent writers with different access patterns on the same file.

Whether this actually hurts performance in practice — i.e., whether I/O interference between fresh writes and compaction writes is measurable — is an empirical question. We formulated a model to figure out when C is actually worth the extra complexity, and ran it before deciding.

---

### Step 6 — Formulating the optimization: what are the costs?

Before committing to either approach, we wanted a formal cost model. Two metrics:

- **WA (write amplification)** — how many times is each user byte written? Once fresh (cost 1), plus once during compaction if the segment containing it gets compacted. Worst case: `WA = 1 + T` where T is the cold threshold.
- **SA (space amplification)** — how much disk does a byte occupy beyond its live size? At worst case utilization T, with headroom fraction f: `SA = 1 / (T * (1 - f))`.

The objective is to minimize `α*WA + β*SA` where α and β express how much you care about each.

**First attempt used CVXPY.** It failed — the product `(1 - f) * T` in the denominator of SA is not DCP-compliant (non-convex in the CVXPY sense when both are decision variables). We had to change the formulation.

**Fix**: the headroom feasibility constraint `f * S ≥ T * (1 - f) * S` gives `f ≥ T / (1 + T)`. Setting `f` to its minimum feasible value eliminates it as a free variable. SA becomes `(1 + T) / T` — now a single-variable problem.

The objective `α*(1 + T) + β*(1 + T)/T` is nonlinear because of the `β/T` term (a hyperbola). It is convex (`d²/dT² = 2β/T³ > 0`), so scipy's bounded scalar minimizer finds the global minimum trivially. We also derived a closed-form: `T* = sqrt(β/α)`, capped at 0.5.

![Objective function vs T for different priority weights](plot_objective_surface.png)

---

### Step 7 — What the optimizer told us about T

Running the closed-form across a range of α/β weights:

- For any α/β ratio where SA matters at all (β/α ≥ 1), `T* = 0.5` is optimal.
- Below that, T* = sqrt(β/α) — it shrinks only when WA is heavily dominant.
- The proposed `T = 0.30` is intentionally conservative relative to `T* = 0.50`. The gap: WA = 1.30 (vs optimal 1.50), SA = 4.33 (vs optimal 3.00). We are paying 44% worse SA to be conservative about compacting temporarily-cold segments.

**Decision to revisit**: bump the proposed threshold closer to T* = 0.45–0.50 after benchmarking confirms real-world utilization distributions.

![WA vs SA Pareto frontier for Approach C](plot_pareto_c.png)

---

### Step 8 — Comparing B and C: when does headroom actually pay off?

The optimization above is for Approach C. But C has a cost: headroom is dead space until filled. At equal T, Approach B has strictly lower SA:

```
SA_B = 1 / T               (no headroom waste)
SA_C = (1 + T) / T         (headroom overhead)

At T = 0.5:  SA_B = 2.0,  SA_C = 3.0   (C is 50% worse)
```

WA is identical in both when there is no I/O interference. So at `ρ = 1`, B always wins.

We modeled interference as a multiplier `ρ ≥ 1` on B's effective WA and solved a bi-level program to find the crossover:

```
α = 0.5, β = 0.5:   ρ_c = 1.67    (B needs to slow writes 67% for C to win)
α = 0.9, β = 0.1:   ρ_c = 1.08    (WA-heavy: any contention tips to C)
α = 0.1, β = 0.9:   ρ_c = 7.00    (Space-heavy: C almost never wins)
```

This is the decision the benchmark has to resolve: measure actual ρ, compare to ρ_c.

![Approach B vs C optimal objective as a function of interference ρ](plot_b_vs_c.png)

---

### Step 9 — Making compaction lock-free

The compaction process is naturally crash-safe and lock-free without any additional coordination:

1. **Copy** live bytes from the cold segment to the destination (pure I/O, no locks).
2. **Commit** all updated offsets in a single SQLite transaction.
3. **Queue** the old segment for deletion; background worker removes it after a grace period.

If the server crashes during step 1: the commit never happened, old offsets are valid, the old file still exists. Compaction restarts on the next GC cycle with no data loss.

For reads: a reader that fetches old offsets from SQLite before the commit continues reading from the old file (which still exists). A reader that fetches after the commit gets new offsets and reads from the new location. The one race — reading old offset, commit fires, file is deleted before the reader opens it — is handled by: (a) the grace period on deletion (in-flight readers finish before the file disappears), and (b) an `ENOENT` retry that re-reads the offset from SQLite, which by that point has the new value.

No mutexes. No read locks. No blocking the writer.

---

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

Each sealed segment reserves `HEADROOM = f * SEGMENT_SIZE` bytes. Compaction survivors are written into the headroom of `active - 1`, never the active segment. This is an architectural choice: the active segment is exclusively for fresh writes. There is no shared resource between the writer and the compactor.

```
Segment N-2:  [==live==|--dead--|  headroom  ]  ← cold target
Segment N-1:  [==live==|--dead--|  headroom  ]  ← compaction destination
Segment N:    [===fresh writes...            ]  ← active, untouched
```

Whether this separation actually prevents I/O contention in practice is an empirical question answered by benchmarks. If contention is negligible (Approach B), C pays a higher SA cost for cleanliness with no throughput benefit.

### Lock-Free Compaction

Compaction is a copy-before-commit operation. No locks are held at any point:

1. **Copy** — read live bytes from cold segment N into headroom of segment N-1 (pure I/O, no locks)
2. **Commit** — update all affected `offset_size_list` entries in a single SQLite transaction
3. **Unlink** — add segment N to the deletion queue; background worker removes it after a grace period

**Crash safety**: the cold segment file is not removed until after the SQLite commit. If the server crashes mid-copy, the commit never happened, old offsets in SQLite are still valid, and the old file still exists. Compaction restarts cleanly on the next GC cycle.

**Read safety without locks**: On Linux, `unlink()` removes the directory entry but keeps the file data accessible through open file descriptors. A reader that opened segment N before the unlink can still read from it. A reader that opens segment N after the commit gets new offsets from SQLite and opens N-1 instead.

The one race: a reader fetches old offset from SQLite, compaction commits and unlinks the file, then the reader tries to open the now-unlinked file and gets `ENOENT`. Fix: on `ENOENT`, re-read the offset from SQLite and retry once. By that point the commit has landed and the new offset points to the correct segment.

### Compaction Process

1. Compute utilization for all sealed segments except `active - 1`
2. Pick the lowest-utilization segment below `COLD_THRESHOLD T`
3. Copy its live bytes into the headroom of `active - 1` (no locks held)
4. Wrap all `offset_size_list` updates in a single SQLite transaction and commit
5. Add cold segment to deletion queue; background worker unlinks it after grace period

---

## Analytical Modeling

We formulate two optimization problems. The first (single-level) finds the optimal parameters for Approach C. The second (bi-level) determines under what conditions Approach C is preferable to Approach B.

### Notation

| Symbol | Meaning |
|---|---|
| `S` | SEGMENT_SIZE (bytes) |
| `T ∈ (0, 0.5]` | Cold threshold — compact segments with utilization below T |
| `f ∈ [0, 0.5]` | Headroom fraction — `f*S` bytes reserved per sealed segment |
| `u ∈ (0, T]` | Utilization of a segment at compaction time; worst case `u = T` |
| `α, β ≥ 0, α+β=1` | Objective weights for WA and SA respectively |
| `ρ ≥ 1` | Write interference factor for Approach B (measured from benchmarks) |

---

### Derivation of WA and SA

**Write Amplification (WA)**

Every user byte is written once as a fresh write (cost 1). When a segment is compacted, the fraction `u` of its data capacity `(1-f)*S` that is still live must be copied to the destination segment. So:

```
WA = 1 + u
```

Worst case is `u = T` (we compact exactly at threshold), giving `WA = 1 + T`.

**Space Amplification (SA)**

In steady state the worst-case segment is one sitting just above the compaction threshold — it has `u*(1-f)*S` live bytes but occupies `S` bytes on disk. Space amplification is the ratio of disk usage to live data:

```
SA = S / (u * (1-f) * S)  =  1 / (u * (1-f))
```

At worst case `u = T`:

```
SA = 1 / (T * (1-f))
```

**Headroom Feasibility**

The headroom `f*S` in `active - 1` must be large enough to absorb the survivors from one cold segment (`T*(1-f)*S` bytes):

```
f * S  ≥  T * (1-f) * S
f      ≥  T / (1 + T)
```

Setting `f = T/(1+T)` (minimum feasible headroom) and substituting into SA:

```
SA = (1 + T) / T
```

This is the key simplification: once headroom is set to its minimum feasible value, both WA and SA are functions of T alone.

---

### Problem 1 — Single-Level: Optimal Parameters for Approach C

We minimize a weighted sum of WA and SA over T:

```
minimize_{T}    α * (1 + T)  +  β * (1 + T) / T
                = (1 + T)(α + β/T)

subject to      0 < T ≤ 0.5
                f = T / (1 + T)          (minimum feasible headroom)
```

This is a **nonlinear convex program** in one variable. The objective contains `β/T` which is nonlinear (hyperbolic), but the second derivative `2β/T³ > 0` confirms convexity for T > 0. We solve it analytically by setting the first derivative to zero:

```
d/dT [(1+T)(α + β/T)]  =  α  -  β/T²  =  0

T* = sqrt(β / α),   capped at 0.5
f* = T* / (1 + T*)
```

**Closed-form results** (T* capped at 0.5 when β/α ≥ 1):

| α | β | T* | f* | WA | SA |
|---|---|---|---|---|---|
| 1.0 | 0.0 | → 0 | → 0 | → 1.0 | → ∞ |
| 0.9 | 0.1 | 0.33 | 0.25 | 1.33 | 4.00 |
| 0.5 | 0.5 | 0.50 | 0.33 | 1.50 | 3.00 |
| 0.1 | 0.9 | 0.50 | 0.33 | 1.50 | 3.00 |
| 0.0 | 1.0 | → ∞ | → 1 | → ∞ | → 1.0 |

The solution is flat for most α/β values: `T* = 0.5, f* = 0.33` is optimal whenever SA matters at all (β/α ≥ 1). This is confirmed numerically by `optimize.py`.

---

### Problem 2 — Bi-Level: Approach B vs Approach C

Approach B compacts into the active segment (no headroom, `f = 0`). Approach C compacts into `active - 1` headroom (`f = T/(1+T)`). Their metrics at equal T are:

```
             WA          SA
Approach B:  1 + T       1 / T              (no headroom waste)
Approach C:  1 + T       (1 + T) / T        (headroom overhead)
```

WA is identical. B has strictly lower SA than C at any T — headroom is dead space until filled. If there is no I/O contention in B, then B dominates C.

The variable that determines which approach to implement is `ρ ≥ 1` — the ratio of write throughput under idle conditions to write throughput while compaction is running concurrently on the active segment:

```
ρ = throughput_idle / throughput_during_compaction
```

`ρ = 1` means no impact. `ρ = 2` means compaction halves fresh write throughput.

We model effective WA inclusive of interference:

```
WA_B_eff = (1 + T) * ρ       WA_C_eff = 1 + T  (no shared segment)
SA_B      = 1 / T             SA_C      = (1 + T) / T
```

We formulate approach selection as a **bi-level program** parameterized by ρ:

```
Upper level:    choose x ∈ {B, C}  to minimize  V*(x, ρ)

Lower level B:  V*(B, ρ) = min_{T ∈ (0, 0.5]}  α*(1+T)*ρ  +  β/T

Lower level C:  V*(C)    = min_{T ∈ (0, 0.5]}  α*(1+T)    +  β*(1+T)/T
```

**Solving** (numerically via `optimize.py`; interior optima shown below):

```
T_B* = sqrt(β / (α*ρ)), capped at 0.5    →  V*(B, ρ)  (from optimizer)
T_C* = sqrt(β / α),     capped at 0.5    →  V*(C)      (from optimizer)
```

**Crossover `ρ_c`** — C is preferred when measured `ρ > ρ_c`:

```
α      β     |  ρ_c    Interpretation
-------|------|----------------------------------------------------
0.9    0.1   |  1.08   WA-dominant: tiny interference favors C
0.7    0.3   |  1.29   C wins at 29% throughput drop
0.5    0.5   |  1.67   Balanced: C wins at 67% throughput drop
0.3    0.7   |  2.56   SA-dominant: need severe contention for C
0.1    0.9   |  7.00   Space-only: B wins in almost all cases
```

At `ρ = 1` (no interference), **B wins across all weightings**. C has higher SA because headroom is dead space that displaces live data. C is only justified if benchmarks show `ρ > ρ_c` for the target workload.

`ρ` must be measured. `optimize.py` computes `V*(B, ρ)` and `V*(C)` for any measured `ρ` and outputs which approach wins and the optimal T.

---

### Benchmark Plan

Benchmarks serve two purposes: validate the model predictions for WA and SA, and measure `ρ` to determine which approach to implement.

1. **Write throughput** — sequential write of 10 GB at object sizes 1 KB / 1 MB / 100 MB. Measure MB/s sustained.
2. **Read latency** — random reads across a 10 GB dataset after compaction. Measure p50/p99 latency and MB/s.
3. **Space reclaimed** — write 10 GB, delete 70%, run one compaction cycle. Measure bytes on disk before/after and wall time.
4. **Measured WA** — instrument total bytes written during workload + compaction. Compare to model prediction `1 + T`.
5. **Write interference `ρ`** — run Approach B with concurrent fresh writes during compaction. `ρ = throughput_idle / throughput_during_compaction`. Compare against `ρ_c` to determine which approach wins.

Results will be added to this RFC. If measured `ρ < ρ_c`, we implement B. If `ρ > ρ_c`, we implement C. The optimizer script `optimize.py` can be re-run with measured `ρ` to refine the recommended constants.

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
