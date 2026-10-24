# Serving user-defined placement policies: architecture, operator cost model, benchmark plan

Status: design. The engine (a sandboxed `WasmPacker` behind `StripePacker`, plus
an always-on `validate_stripes` + degrade-to-plain gate in the coordinator) is
built and tested behind the `wasm` feature. This document covers what it takes
to let a *user* create a policy-bound bucket on a *live* cluster, what that
costs us as the operator, and the micro/macro benchmarks that will decide each
open design choice with numbers rather than assertion.

The governing principle: **transparent and accountable by construction.** Every
cost a user's policy imposes — storage overhead, per-PUT compute, durability
blast radius — is measured, bounded, and attributable before it is incurred. A
user gets Fusion-class placement control and cannot hurt another tenant or
themselves; the operator never absorbs an unbounded cost.

## 1. Current state and the gaps to self-serve

Works today: an operator pre-loads one module (`WARPDRIVE_WASM_PACKER=path`) as
the global `"wasm"` packer; a bucket selects it by `packer_name`. `bucket_config`
stores the *name* and an overhead threshold — not the module.

Missing for a user to do this per bucket on a live cluster:
1. **Submit API** on the authenticated S3 surface (`PutBucketPlacementPolicy`),
   not the bare pre-auth `_admin` route.
2. **Store the module bytes in the replicated `bucket_config`** so any node
   (any node can coordinate a read or write) can load that bucket's policy.
3. **Per-bucket module instances**, keyed by `(bucket, content-hash)`, not one
   global entry.
4. **Submit → simulate → explain → gate**: the bucket binds the policy only
   after a sandboxed sample run reports an accepted cost.

## 2. Self-serve architecture

- **Policy identity = content hash.** A submitted module is addressed by the
  SHA-256 of its bytes. `bucket_config` stores `{packer_name:"wasm",
  policy_hash, overhead_threshold}` plus the bytes in a content-addressed blob,
  replicated the same way placement pins already are (quorum write). Dedup is
  free (same bytes → same hash). A module-size cap (e.g. 1 MiB) bounds both the
  blob store and compile cost.
- **Per-node compiled-module cache.** Keyed by `policy_hash`. First PUT/read on
  a node that touches the bucket compiles once (wasmtime `Module`); a bounded
  LRU holds compiled artifacts. Eviction → recompile (cold start). Pre-compile
  at submission so the first real PUT is never cold. **Benchmark M3/M4 size this
  cache and quantify cold vs warm.**
- **Validation is layered and already correct:** sandbox (no imports, fuel,
  epoch, memory) on every invocation, then `validate_stripes` on every output,
  then degrade-to-plain. None of this changes for self-serve.

## 3. Serving runtime — the perf mechanisms, and why each needs a number

The prototype proved correctness but made choices that do not survive
production load. Each is a benchmark, not an opinion.

| Mechanism | Prototype does | Production needs | Benchmark |
|---|---|---|---|
| Epoch deadline | spawns **one thread per `pack`** | one shared epoch ticker thread for the whole process | **M1** |
| Instance lifecycle | fresh `Store`+`Instance` per `pack` | measure fresh vs pooled; keep fresh if isolation is cheap enough | **M2** |
| Module compile | compile per load | compile once, cache; measure cold vs warm | **M3/M4** |
| Sandbox limits | fuel+epoch+`StoreLimits` always | confirm the limit machinery itself is not the cost | **M5** |
| Fairness | none | per-tenant concurrency cap + off-request-thread pool | **M6** |

## 4. Operator cost model

What we own the moment a user's policy is live, and the bound we hold on each:

- **Storage overhead is user-chosen** (up to 200%+). Bound: `overhead_threshold`
  is enforced (degrade to plain above it), and materialized overhead is metered
  per bucket and billed. **Benchmark A3** measures overhead actually written to
  disk per policy vs the report's prediction.
- **Durability is heterogeneous per bucket.** Co-location raises the
  correlated-loss blast radius (objects per stripe). Bound: the blast radius is
  disclosed at submission and capped per durability tier; repair prioritizes
  high-blast stripes. **A4** measures objects-lost-per-stripe-loss by policy.
- **Metadata/location-map bloat.** Fine-grained packing records more per object.
  Bound: a per-object metadata budget; reject policies that explode unit counts.
  **A5** measures location-store bytes/object by policy at scale.
- **GC holes.** Shared stripes can't be freed until all members die or a
  compaction re-runs placement. **A6** measures reclaimable-vs-dead space under
  a delete workload by policy.
- **Repair cost.** Re-encode must honor per-bucket layout (recorded, not
  re-run). Bound: always record the layout; never re-execute user WASM on the
  repair path.

## 5. Accountability surface

Per bucket, continuously: materialized overhead %, p50/p99 per-PUT policy
compute (fuel + wall), blast radius, metadata bytes/object. The same `explain()`
report, now fed by production counters, not just a sample — so the number a user
saw at submission is reconciled against what they actually incur, and billing
derives from it.

## 6. Benchmark plan

Micro (criterion, `server/benches/`, isolate one mechanism each):

- **M1 — epoch ticker.** thread-per-`pack` vs one shared ticker: per-`pack`
  latency and threads created/sec at 1/8/64 concurrent packers. *Decision: adopt
  shared ticker if it removes measurable per-call cost.*
- **M2 — instance lifecycle.** fresh `Store`+`Instance` vs pooled, warm module:
  packs/sec and p99. *Decision: pool only if fresh is the bottleneck.*
- **M3 — cold start.** compile+instantiate (cold) vs cached module (warm):
  first-PUT penalty. *Decision: whether pre-compile at submission is mandatory.*
- **M4 — module cache.** warm-hit vs LRU-miss (recompile) latency, and compiled
  bytes per module. *Decision: cache capacity.*
- **M5 — limit overhead.** pack with vs without fuel/epoch/`StoreLimits`.
  *Decision: confirm safety machinery is ~free.*
- **M6 — noisy neighbor.** 1 tenant vs N tenants sharing cores, with/without a
  bounded pool: throughput and cross-tenant p99. *Decision: pool sizing.*

Macro (against a live RS(3,2) cluster, reuse the jepsen/load harnesses):

- **A1 — PUT hot-path tax.** throughput/latency: plain vs FacPacker vs WASM
  policy, swept over units/object. *Is the per-PUT tax acceptable end to end?*
- **A2 — read/pushdown benefit.** stripes-touched and bytes-read for grouped
  queries: workload-aware WASM policy vs FacPacker. *The Fusion-style payoff,
  measured.*
- **A3 — materialized overhead** on disk per policy vs predicted.
- **A4 — blast radius** realized under injected stripe loss (reuse the chaos
  nemesis) per policy.
- **A5 — metadata footprint** (location-store bytes/object) by policy at 10^4–10^6
  objects.
- **A6 — GC/compaction** reclaim under a delete workload by policy.

Each macro result is appended to `docs/PERF-JOURNAL.md` in the project's house
style (real numbers, the reasoning, dead ends kept).
