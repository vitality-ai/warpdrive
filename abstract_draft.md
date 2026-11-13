# Per-Object Placement Knobs: Generalizing Workload-Aware Erasure Coding Beyond Parquet

**Track:** HiPC 2026 Call for Posters (general track) · 2 pages, IEEE double-column, 10pt

---

## 1. Introduction (~0.4 page)

Erasure-coded object stores treat objects as opaque byte blobs, striping them into
fixed-size blocks with no awareness of the structure inside. Fusion [Lu et al.,
ASPLOS'25] showed this is costly: when a stripe boundary splits a Parquet column
chunk — the smallest unit a query can evaluate independently — across nodes, the
storage layer must reassemble it over the network before a predicate can even run,
undoing the point of computation pushdown. Fusion's fix, file-format-aware coding
(FAC), aligns stripe construction to column-chunk boundaries via a bin-packing
algorithm, cutting storage overhead to ~1.2% above optimal while eliminating
fragmentation.

FAC's alignment rule, however, is hardcoded to one format. We ask: what happens when
the same mechanism is exposed as a **per-object, workload-tunable knob** instead of a
format-specific constant — and does it generalize to storage layouts that don't look
like Parquet at all? We build a generalized FAC core and a knob layer on top of it,
and evaluate both on Parquet (Fusion's own case, for direct comparison) and on
Neon-style database delta/image WAL layers — a row/byte-range-oriented layout with no
column structure whatsoever. This work targets WarpDrive, an S3-compatible object
store built for computation pushdown [cite: WarpDrive CMDS'26]; the knob mechanism
described here is prototyped standalone and not yet integrated into WarpDrive's
server.

## 2. Idea & Design (~0.6 page)

**Generalizing the computable unit.** We define the *computable unit* as the
smallest object fragment a query can evaluate independently — for Parquet, a column
chunk; for Neon layers, a whole layer file (the pageserver already reads whole
layers to reconstruct a page version; there is no meaningful sub-layer fragment).
The same abstraction therefore means two different concrete things per format: a
**splitting** constraint for Parquet (never break a column chunk across a stripe)
and a **grouping** decision for Neon layers (how many whole layers to bundle into
one stripe). We generalize Fusion's Algorithm 1 (largest-item-seeds-the-stripe,
greedy least-occupied-bin packing) to take an arbitrary `(unit_id, size)` list and
stripe width `k`, with no format-specific logic in the core packer.

**The knob.** Per object, a client declares: (i) the computable-unit granularity
(e.g., Parquet: column-chunk / row-group / whole-file; Neon: per-layer / batch-of-N
layers), and (ii) the EC parameters `(k, m)`. Crucially, the *mechanism* by which
WarpDrive learns the computable-unit boundaries is a client-supplied header at PUT
time (`x-warpd-computable-units: [[offset,len],...]`) — not a server-side format
parser as in Fusion. This is what actually makes the generalization real: the server
never needs per-format code, only the boundary list, which the client already has
(pyarrow exposes column-chunk offsets it just wrote; the Neon pageserver already
knows its own layer boundaries).

**Two metrics in tension.** We measure (a) storage overhead vs. optimal (Fusion's
own metric) and (b) two read-side costs that pull in *opposite* directions as
granularity varies: byte read amplification (bytes fetched vs. bytes actually
needed for a query) and round trips (distinct bins touched per query — the network-
RTT-bound latency driver Fusion's own motivation centers on). Finer granularity
wastes fewer bytes per bin but can touch more distinct bins; coarser granularity
does the reverse. This is the real Pareto frontier the knob lets a workload navigate.

## 3. Preliminary Results (~0.6 page)

**Setup.** A synthetic TPC-H-lineitem-shaped Parquet table (16 columns, 10 row
groups, 500,000 rows — matching Fusion's own evaluated file shape) and a synthetic
population of 5,000 Neon delta/image layers, sized against real Neon pageserver
source (`DEFAULT_CHECKPOINT_DISTANCE` = 256MiB target file size, used identically in
both the L0-flush and gc-compaction paths; the compactor itself flags >2x that
target as anomalous, giving a principled upper bound for our size distribution) and
a real delta:image event ratio (3.86:1) pulled from WarpDrive's own `scaling_slab`
benchmark logs. RS(9,6), median over N=25 trials per configuration with
[25th,75th] percentile bands, swept over three query-selectivity regimes
(1%/10%/50%) and a dense sweep of granularity-knob settings per format (batch sizes
1 through the whole object — 8 settings for Parquet, 9 for Neon — rather than a
few hand-picked levels, so the frontier below is a real curve, not three points).

**Reproduction check.** FAC at Parquet column-chunk granularity reaches
**1.16%** storage overhead vs. optimal in our synthetic setup — the same order of
magnitude as Fusion's own reported ~1.2% on their real 10GB file, validating the
generalized packer before trusting anything built on top of it.

**Finding 1 — the same granularity choice costs wildly different amounts on
different formats.** At a matched batch size of 16 computable units per storage
unit (exactly one row group for Parquet; an arbitrary equal-sized batch for Neon
layers), Parquet's storage overhead is **79.99%** while Neon's is **1.20%** — a
~67x difference in overhead cost from applying the identical granularity setting
to two formats. Fusion's own choice (finest, column-chunk) costs Parquet only
1.16%; Neon's finest (per-layer) costs 0.19%. Neither format's cost curve predicts
the other's. The "right default" granularity is format-dependent, not a universal
constant — which is itself the argument for exposing it as a per-object knob
rather than hardcoding it, the way Fusion does.

**Finding 2 — round trips and bytes trade off, and the winner flips with query
selectivity.** At 50% selectivity on Parquet, the finest granularity (column-chunk)
costs 71.9 round trips at 23.4% byte overhead, while the coarsest
(whole-file) costs a single round trip at 101.9% byte overhead — worse on bytes,
dramatically better on round trips. At 1% selectivity the same two extremes are
2.0 round trips / 114.8% bytes vs. 1.0 round trip / 19,040% bytes — an order of
magnitude further apart. Which point on the curve is "correct" depends on whether
the workload is latency-bound (round trips dominate, matching Fusion's own stated
motivation) or bandwidth-bound (bytes dominate) — a distinction a fixed, one-size
policy cannot express, but a per-object knob, swept densely rather than pinned to
one or two presets, can.

*(See attached Figure 1: round trips vs. byte amplification, both formats, three
selectivity regimes, log-log. Figure 2: storage overhead by granularity, Parquet
vs. Neon layers.)*

## 4. Conclusion & Future Work (~0.3 page)

Generalizing Fusion's fixed alignment rule into a per-object tunable knob preserves
its benefit on the format it was designed for and extends it to a structurally
different one, while surfacing a genuine, workload-dependent tradeoff (round trips
vs. bytes) that a fixed policy cannot navigate. This is a standalone prototype;
integrating the knob mechanism into WarpDrive's server (the `x-warpd-computable-
units` header path, a persisted per-object policy, and real erasure-coded storage
rather than a simulated packer) is ongoing work, alongside evaluating on real
Neon pageserver traffic rather than a source-grounded synthetic layer population.

---

## Citations to include
- Lu, Raina, Cidon, Freedman. "Fusion: An Analytics Object Store Optimized for
  Query Pushdown." ASPLOS 2025.
- WarpDrive CMDS'26 position paper (cite once the camera-ready/DOI is available;
  currently: https://vldb.org/2026/Workshops/VLDB-Workshops-2026/CDMS/CDMS26_8.pdf)
- Neon pageserver source, `pageserver/src/tenant/timeline/compaction.rs`
  (neondatabase/neon, for the checkpoint-distance/target-file-size grounding)

## TODO before submission
- [x] Fill in numbers from the full-scale run (results.json: 500k-row Parquet,
      5,000-unit Neon, dense granularity sweep, run on openaurora, 27m25s)
- [ ] Insert Figure 1 (pareto_both_formats.png) and Figure 2 (overhead_by_granularity.png)
- [ ] Trim to fit 2 pages in the actual IEEE two-column template (this draft is
      sized for that budget but needs a real layout pass)
- [ ] Optional author/affiliation line (CFP says optional)
- [ ] AI-disclosure acknowledgement per IEEE guidelines (required by the CFP)
- [ ] Submit via https://ssl.linklings.net/conferences/HiPC/ before Oct 1 AOE
