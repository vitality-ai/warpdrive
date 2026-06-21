# RFC 3 — Segment-Based Storage with Space Reclamation

**Status:** Draft  
**Branch:** `feat/rfc-segment-storage`

---

## Problem

The current storage layer is a single append-only file per user/bucket. Object location is stored as `(offset, size)` extents in SQLite. Deletes queue the extents via the deletion WAL but the background worker has no way to actually reclaim space — the file grows forever with holes.

There is no compaction, no merging, and no way to shrink disk usage after objects are deleted or overwritten.

---

## Goals

- **Writes stay O(1) and append-only** — no seek-before-write, no in-place mutation; write throughput must not regress
- **Reads stay O(1)** — a read is still a direct seek by offset within a file; no extra indirection
- **Space is actually reclaimed** — deleted object extents are freed by removing whole segment files

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

> **Checkpoint 1 — Problem established**
> The current flat-file layout has no space reclaim path. Deletes remove the SQLite row but bytes stay on disk forever. In-place compaction is ruled out: it invalidates all offsets, is unsafe under crashes, and blocks reads. We need a layout where freeing space means deleting a whole file.

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

Each block is a 512 MB file on disk. The arrow shows how a single number — the global offset stored in SQLite — tells you exactly which file to open and where to seek inside it. No new columns, no mapping table. Reads just do two integer operations (divide and modulo) before opening the file.

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

The orange region is wasted disk space — bytes that are dead but still occupy storage. In Phase 2, deleting 70% of the data drops live bytes immediately but disk usage stays flat. That gap is space amplification in action. Compaction in Phase 3 closes it by copying survivors out of cold segments and deleting those segment files entirely. Without compaction the orange region grows forever as the workload continues.

---

> **Checkpoint 2 — Storage structure decided**
> Storage splits into fixed-size 512 MB segment files per bucket. No schema change needed: the existing global offset encodes the segment file and seek position via integer divide and modulo. Segment utilization is computed on demand from the `objects` table — no counters, no new tables, no drift on crash. A segment is "cold" when the live bytes queried from SQLite fall below threshold T.

---

### Step 5 — Where do survivors go?

When we compact a cold segment, the live bytes have to go somewhere. Two options:

**Option A** — compact into the active segment. Simple: survivors are just appended like any other write. No structural change needed.

**Option B** — reserve headroom in `active - 1`, compact there. This completely separates fresh writes (active) from compaction writes (active - 1). The active segment is never touched by the compactor.

The motivation for B is architectural clarity: the active segment has a single writer and a single write pattern (append-only, sequential). Adding compaction writes would mean two concurrent writers with different access patterns on the same file.

Whether this actually hurts performance in practice — i.e., whether I/O interference between fresh writes and compaction writes is measurable — is an empirical question. We formulated a model to figure out when B is actually worth the extra complexity, and ran it before deciding.

---

### Step 6 — What does compaction actually cost?

Before choosing between A and B we needed to understand what we were optimising. Two costs matter:

**SA (space amplification)** is the primary concern — it directly determines how much disk you need to provision. SA = 2 means you need twice the raw disk compared to your live data size. For Option A with no headroom, `SA = 1/T`. To minimise SA you want T as high as possible.

**WA (write amplification)** is a background cost — compaction copies bytes during a GC cycle. If that cycle runs without competing with fresh writes, WA does not affect user-visible write latency. It does consume disk bandwidth and SSD write cycles, so it is not completely free, but it is a second-order concern compared to SA.

Starting from this: if WA were entirely free, the answer is trivially T → 1 (compact as aggressively as possible, SA → 1). In practice T = 1 means compacting after every single delete, so we cap it at 0.5 as a practical upper bound. At T = 0.5, SA = 2.0 for Option A. That is the baseline.

Once we account for WA as a background I/O cost, we introduce weights α and β to express the trade-off. The optimizer then finds the best T — see the Analytical Modeling section for the full derivation.

![Objective function vs T for different priority weights](plot_objective_surface.png)

Each curve is a different priority weighting. The dots show optimal T for each weighting. SA-heavy and balanced weightings both hit T = 0.5 — the optimizer always pushes toward aggressive compaction because that is the only way to reduce SA. WA-heavy weightings pull T down because background compaction I/O becomes costly. Our proposed T = 0.30 (red line) is conservative — we accepted worse SA to avoid compacting segments that might only be temporarily cold.

---

> **Checkpoint 3 — Cost model and Option A baseline established**
> SA = 1/T for Option A. To minimise SA, set T as high as practically possible — T = 0.5 gives SA = 2.0. WA is a background cost; it does not affect write latency unless it competes with fresh writes on the same file. The optimizer confirms T = 0.5 is optimal for most weightings. Our proposed T = 0.30 is intentionally conservative and should be revisited after benchmarking.

---

### Step 7 — What the optimizer told us about T

Running the cost model across a range of α/β weights:

- For any weighting where SA matters at all (β/α ≥ 1), `T* = 0.5` is optimal.
- WA-heavy weightings pull T* down: T* = sqrt(β/α).
- The proposed T = 0.30 gives SA = 4.33 vs optimal SA = 3.00 at T = 0.5. We are paying 44% worse SA to be conservative.

**Decision to revisit**: bump T closer to 0.45–0.50 after benchmarking confirms real-world utilization distributions.

![WA vs SA Pareto frontier for Approach B](plot_pareto_c.png)

Each point on the frontier is an operating point — a choice of T giving a specific WA/SA trade-off. The frontier collapses to a single point (T = 0.5, SA = 3.0) for most weightings because the optimizer always hits the cap. The red point (proposed T = 0.30) sits off the frontier with higher SA than optimal.

---

### Step 8 — Does WA cause interference? This is when headroom matters.

So far we have been treating WA as a pure background cost. But in Option A, compaction writes go into the active segment — the same file fresh writes are landing in. The OS and storage device have to interleave two concurrent write streams on the same file. This can slow fresh write throughput.

We call this **ρ** — the ratio of write throughput at idle to write throughput while compaction is running:

```
ρ  =  throughput_idle / throughput_during_compaction
```

Concretely: if your server writes at 500 MB/s normally and 300 MB/s while compaction is running, ρ = 500/300 = 1.67. ρ = 1 means compaction has no impact at all. ρ = 2 means fresh write throughput is cut in half every time a compaction cycle runs.

When ρ > 1, WA stops being a background cost and starts being a write latency problem. This is exactly what Option B solves: by routing compaction into the headroom of active-1, the active segment is never touched during compaction. Fresh writes and compaction writes never compete. For Option B, ρ = 1 always.

The trade-off: Option B eliminates the ρ penalty but pays a higher SA (SA_B = (1+T)/T vs SA_A = 1/T — 50% worse at T = 0.5). So the question is: is the SA overhead of B worth paying to eliminate the ρ penalty of A?

```
α = 0.5, β = 0.5:   ρ_c = 1.67    (compaction must slow writes 67% before B wins)
α = 0.9, β = 0.1:   ρ_c = 1.08    (write-throughput-critical: any interference tips to B)
α = 0.1, β = 0.9:   ρ_c = 7.00    (space-critical: B almost never worth the SA overhead)
```

![Approach A vs B optimal objective as a function of interference ρ](plot_b_vs_c.png)

Left: as ρ increases from 1, Option A's effective cost rises while B's stays flat. They cross at ρ_c. Below the crossover A wins (lower SA, and ρ hasn't hurt enough). Above it B wins (eliminating ρ outweighs the SA overhead). Right: ρ_c varies sharply by priority — write-throughput-critical workloads tip to B almost immediately; space-critical workloads almost never find B worth it.

**Workload conclusions:**

- **Write-latency-sensitive workloads** (serving live traffic, real-time ingestion): ρ is a real concern. Even moderate contention tips to B. Prefer B.
- **Batch / archive workloads** (backups, cold storage, periodic ingestion): write latency does not matter. ρ is irrelevant. Prefer A — you get better SA at no cost.
- **High churn workloads** (many deletes, frequent overwrites): segments go cold fast, compaction runs frequently, ρ compounds over time. Lean toward B.
- **Low churn workloads** (mostly writes, few deletes): segments stay warm for a long time, compaction is infrequent, ρ rarely occurs. A is fine.

**Tuning T by workload:**

T controls how cold a segment has to be before you compact it. The right T depends on your delete rate:
- High delete rate: segments go cold quickly regardless. Lower T means fewer false-cold compactions but doesn't change much. T = 0.4–0.5 is reasonable.
- Low delete rate: segments accumulate dead bytes slowly. A conservative T = 0.2–0.3 avoids compacting segments that are only temporarily cold because of a burst of deletes.

T can be user-configured or made adaptive — the system can observe the rolling delete rate per bucket and adjust T automatically. We start with a static T = 0.30 and revisit once we have production data.

---

> **Checkpoint 4 — Approach decision deferred to benchmarks**
> Option A (no headroom) has lower SA. ρ determines whether its WA becomes a write latency problem. Option B eliminates ρ at the cost of 50% worse SA. The benchmark measures ρ for the target workload and compares it to ρ_c. Write-latency-sensitive or high-churn workloads favour B; batch and low-churn workloads favour A. T should be tuned to the observed delete rate — higher T for high churn, lower T when deletes are bursty or infrequent.

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

> **Checkpoint 5 — Design complete**
> We have a full design ready to implement. Segment files give us space reclamation without schema changes. Utilization comes from SQLite for free. The compaction process is crash-safe and lock-free: copy survivors → commit offsets atomically → delete old segment after a grace period. The one open question is whether to implement A or B, which the benchmark resolves by measuring ρ. Everything else — segment size, headroom, cold threshold — has optimal values derived from the cost model and can be tuned after benchmarking.

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

Whether this separation actually prevents I/O contention in practice is an empirical question answered by benchmarks. If contention is negligible (Approach A), B pays a higher SA cost for cleanliness with no throughput benefit.

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

This section builds the cost model in stages. Each stage adds one constraint, arriving at a decision rule for when to prefer Option B over Option A.

**Symbols:**
- `T` — cold threshold (0 to 0.5): compact a segment when live utilization falls below this fraction
- `f` — headroom fraction: what fraction of each segment is reserved for compaction writes
- `S` — segment size (512 MB)
- `α, β` — priority weights for WA vs SA (α + β = 1)
- `ρ` — write interference: ratio of idle write throughput to write throughput during compaction

---

### Stage 1 — SA is the primary concern

If compaction runs in the background without affecting fresh writes, WA does not touch user-visible write latency. The only cost that matters is SA — how much disk you need.

For Option A (no headroom), the worst-case moment for SA is a segment sitting just above the cold threshold — T fraction of it is still live, the rest is dead bytes waiting to be reclaimed:

```
SA_A  =  1 / T
```

To minimise SA you maximise T. T = 1 means compact after every single byte dies — constant compaction. The practical cap is T = 0.5: only compact when the majority of a segment is dead. At T = 0.5: `SA_A = 2.0` — the best Option A can achieve, using 2× the disk of your live data size.

---

### Stage 2 — Where WA comes from

**Write amplification** is how many times each user byte gets written in total. Every byte is written once fresh. After that, it may be copied again if its segment gets compacted — and then again if the segment it moved to also eventually goes cold.

We model this with two parameters:
- `T` — the cold threshold, which controls how aggressively we compact
- `L` — object lifetime measured in "segment-fill cycles" (how many times a fresh segment fills up during the object's life). `L = 1` means the object lives roughly as long as one segment takes to fill; `L = 10` means it survives ten fill cycles.

When a segment is compacted, T fraction of its bytes are still alive and get copied. In the next segment, the same logic applies — with probability T the byte survives to the next compaction. So the expected number of compactions k a byte goes through is:

```
k(T, L) = T × (1 - T^L) / (1 - T)

WA = 1 + k
```

The two limiting cases:
- `L = 1` (short-lived objects) → `k = T`, so `WA = 1 + T`
- `L → ∞` (objects live forever) → `k = T/(1-T)`, so `WA = 1/(1-T)`

In practice WA sits somewhere between these depending on how long your objects live relative to your compaction rate.

![WA ablation over object lifetime L and cold threshold T](plot_wa_ablation.png)

Left: each curve is a different object lifetime L. Short-lived objects (L = 1, bottom curve) are mostly deleted before their segment goes cold, so they rarely get compacted more than once — WA is low. Long-lived objects (L → ∞, top dashed curve) keep getting re-compacted every time their segment goes cold, driving WA toward 1/(1-T). The red and gray verticals mark the proposed (T = 0.30) and optimal (T = 0.50) operating points. Right: for a fixed T, k grows and then flattens as L increases — past a certain lifetime the byte almost always sees at least one compaction per cycle, so k converges to its asymptote T/(1-T).

The benchmark measures actual WA directly as total bytes written ÷ total user bytes written, which captures all of this without needing to know L. The model gives us the shape of the relationship; the benchmark pins the exact number.

---

### Stage 3 — WA as a real background cost; adding it to the objective

Even when compaction doesn't interfere with fresh writes, it does consume disk bandwidth and SSD write cycles — costs that matter for hardware longevity and peak storage load. We introduce weights α and β to express the trade-off between WA and SA:

```
minimize over T:    α × WA  +  β × SA
```

For Option A with no headroom (f = 0), SA = 1/T and WA = 1 + k(T, L). Using the L = 1 approximation (one compaction per byte lifetime) as a baseline, WA ≈ 1 + T. The optimizer finds T* = sqrt(β/α), capped at 0.5 — see the objective surface plot above. For any weighting that cares at all about space, T = 0.5 is optimal.

---

### Stage 4 — Option B: headroom and its SA cost

Option A routes compaction writes into the active segment. Option B separates them by reserving headroom in `active - 1`. The headroom introduces an additional SA overhead.

**SA formula for Option B.** With headroom fraction f:

```
SA  =  1 / (T × (1-f))
```

**SA for Option A** is just this with f = 0: `SA_A = 1/T`.

**Concrete example.** 512 MB segment, T = 0.5, f = 0 (Option A):

```
Live bytes  = 0.5 × 512 MB  =  256 MB
SA  =  512 / 256  =  2.0
```

For every 1 MB of real data, you occupy 2 MB of disk — the other 1 MB is dead bytes waiting to be reclaimed.

Now add headroom, T = 0.5, f = 0.33 (Option B):

```
Data capacity  = (1 - 0.33) × 512 MB  =  342 MB
Live bytes     = 0.5 × 342 MB          =  171 MB
SA  =  512 / 171  =  3.0
```

SA jumped from 2.0 to 3.0 purely because of headroom — 170 MB per segment is permanently reserved for compaction writes.

**What about lower T?** At T = 0.3 (Option A, f = 0):

```
Live bytes  =  0.3 × 512 MB  =  154 MB
SA  =  512 / 154  =  3.3
```

Lower T tolerates more dead bytes before compacting, which makes SA worse. This confirms Stage 1: for Option A, always push T as high as practical.

**Tying f to T.** Headroom must fit all survivors from one compaction. A cold segment has `T×(1-f)×S` live bytes that need to land in headroom `f×S`:

```
f×S  ≥  T×(1-f)×S
  f  ≥  T / (1 + T)
```

At T = 0.5: f ≥ 0.33. Setting f to its minimum feasible value:

```
SA_B  =  1 / (T × (1 - T/(1+T)))  =  (1 + T) / T
```

At T = 0.5: SA_B = 1.5/0.5 = 3.0 ✓. At T = 0.3: SA_B = 1.3/0.3 = 4.3.

Now both WA and SA for both options are functions of T alone.

---

### Finding the best T for Option B

We want to minimise the weighted objective for Option B. The optimizer uses the L = 1 approximation for WA as a baseline:

```
minimize over T:    α × (1 + T)  +  β × (1 + T) / T

subject to:         0 < T ≤ 0.5
```

Setting the derivative to zero gives a clean closed form:

```
T* = sqrt(β / α),   capped at 0.5
f* = T* / (1 + T*)
```

What this says intuitively: if you care mostly about space (high β), you want a high T — compact aggressively and accept more write overhead. If you care mostly about write throughput (high α), you want a lower T — compact only when segments are very cold. The cap at 0.5 means the answer is T = 0.5 for most practical weightings:

| Priority | α | β | T* | f* | WA | SA |
|---|---|---|---|---|---|---|
| Only WA | 1.0 | 0.0 | → 0 | → 0 | → 1.0 | → ∞ |
| WA-heavy | 0.9 | 0.1 | 0.33 | 0.25 | 1.33 | 4.00 |
| Balanced | 0.5 | 0.5 | 0.50 | 0.33 | 1.50 | 3.00 |
| SA-heavy | 0.1 | 0.9 | 0.50 | 0.33 | 1.50 | 3.00 |
| Only SA | 0.0 | 1.0 | → ∞ | → 1 | → ∞ | → 1.0 |

The pure extremes (only WA or only SA) are degenerate — one wastes all disk, the other rewrites all data constantly. For anything in between, T = 0.5 and f = 0.33 is the answer.

---

### Stage 5 — When does WA cause interference? Comparing A vs B.

At equal T, both options produce the same WA. They only differ in SA:

```
Option A (no headroom):   SA = 1 / T
Option B (with headroom): SA = (1 + T) / T

At T = 0.5:  SA_A = 2.0,  SA_B = 3.0   (B is 50% worse)
```

B always has worse SA. The only reason to choose B is if compaction into the active segment (Option A) measurably slows fresh writes.

In Option A, compaction writes go to the same file as fresh writes. The OS and storage device interleave two concurrent write streams on the same inode. We measure the interference as:

```
ρ  =  throughput_idle / throughput_during_compaction
```

If your server writes at 500 MB/s idle and 300 MB/s during compaction, ρ = 1.67. ρ = 1 means no impact. ρ = 2 means fresh writes are cut in half every compaction cycle.

When ρ > 1, WA stops being a background cost and starts affecting user-visible write latency. Option B eliminates this by routing compaction into `active - 1` — the active segment is never touched, so ρ_B = 1 always.

We solve each option for its optimal T independently and compare their minimum total costs:

```
Cost_A(ρ) = minimized over T:  α×(1+T)×ρ  +  β/T
Cost_B     = minimized over T:  α×(1+T)    +  β×(1+T)/T
```

B is worth adopting when `Cost_A(ρ) > Cost_B`, which happens past a crossover ρ_c:

| α | β | ρ_c | Plain English |
|---|---|---|---|
| 0.9 | 0.1 | 1.08 | Any interference at all tips to B |
| 0.5 | 0.5 | 1.67 | B wins if compaction slows writes by 67% |
| 0.1 | 0.9 | 7.00 | B almost never wins — space overhead too high |

At ρ = 1 (no interference), Option A always wins. The benchmark measures ρ and places it on this table.

---

### Stage 6 — Workload conclusions and tuning T

The model gives us a decision framework, not a fixed answer. The right choice depends on the workload.

**Which option to use:**

| Workload type | ρ expected | Recommendation |
|---|---|---|
| Write-latency-sensitive (live traffic, real-time ingestion) | Potentially high | Prefer B — eliminate ρ risk even at SA cost |
| Batch / archive (backups, cold storage) | Low — writes are not latency-sensitive | Prefer A — better SA, ρ doesn't matter |
| High churn (frequent deletes, overwrites) | Elevated — compaction runs often | Lean toward B |
| Low churn (mostly writes, rare deletes) | Low — compaction is infrequent | A is fine |

**How to tune T:**

T controls how dead a segment has to be before you compact it. The right T tracks your delete rate:

- **High delete rate**: segments go cold quickly regardless of T. A higher T (0.4–0.5) gives better SA without compacting segments that are still filling up.
- **Low delete rate / bursty deletes**: segments may temporarily look cold after a burst then recover. A conservative T (0.2–0.3) avoids unnecessary compaction.

T can be static (user-configured) or dynamic (the system observes the per-bucket delete rate and adjusts T automatically). We start with T = 0.30 as a conservative static value and revisit once we have production data. Bumping to T = 0.45–0.50 after benchmarking is the expected outcome if the utilization distribution is stable.

---

### Benchmark Plan

Benchmarks serve two purposes: validate the model predictions for WA and SA, and measure `ρ` to determine which approach to implement.

1. **Write throughput** — sequential write of 10 GB at object sizes 1 KB / 1 MB / 100 MB. Measure MB/s sustained.
2. **Read latency** — random reads across a 10 GB dataset after compaction. Measure p50/p99 latency and MB/s.
3. **Space reclaimed** — write 10 GB, delete 70%, run one compaction cycle. Measure bytes on disk before/after and wall time.
4. **Measured WA** — instrument total bytes written during workload + compaction. Compare to model prediction `1 + T`.
5. **Write interference `ρ`** — run Approach A with concurrent fresh writes during compaction. `ρ = throughput_idle / throughput_during_compaction`. Compare against `ρ_c` to determine which approach wins.

Results will be added to this RFC. If measured `ρ < ρ_c`, we implement A. If `ρ > ρ_c`, we implement B. The optimizer script `optimize.py` can be re-run with measured `ρ` to refine the recommended constants.

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
