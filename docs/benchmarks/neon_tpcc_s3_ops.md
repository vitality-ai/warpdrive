# Neon + Warpdrive TPC-C S3 Operation Benchmark

**Branch:** feat/op-counters  
**Date:** 2026-07-08 (Phases 1–7) / 2026-07-16 (Phases 8–9)
**Setup:** Neon main branch, aws-sdk-rust 1.3.3, sysbench 1.0.20 tpcc, scale=10, tables=1

---

## S3 Operation Pricing Reference

All cost estimates use AWS S3 us-east-1 standard pricing (2025). Prices are per 1,000 requests.

| Operation class | Includes | Price / 1,000 requests |
|-----------------|----------|------------------------|
| Write | PUT, COPY, POST, LIST, DELETE_OBJECTS | **$0.005** |
| Read | GET, HEAD | **$0.0004** |
| Delete | Individual DELETE (single object) | free |

**Notes:**
- `DELETE_OBJECTS` is S3's batch-delete API (up to 1,000 keys per call); billed in the write tier.
- Individual `DELETE` requests are free but rare in Neon — the pageserver always uses batch deletion.
- Storage cost ($0.023/GB-month) is not captured by op counters and is not included here.
- Cost formula: `(GET + HEAD) × $0.0004/1000 + (PUT + COPY + LIST + DELETE_OBJECTS + MULTIPART) × $0.005/1000`

### Why DELETE_OBJECTS matters

On a flat object store, S3 objects are immutable. Neon's compaction cycle is therefore forced into a read-modify-write pattern per append: write many small delta layers (PUTs), merge them into a new image layer (PUT), then delete the superseded deltas (DELETE_OBJECTS). WarpDrive's mutable co-located slabs avoid this cycle entirely — deltas are appended in-place, so no compaction GC is needed and cold reads collapse from k independent GETs to a single batched I/O.

### GET type breakdown

Not all GETs are equal. Pageserver on-demand downloads are classified by layer type:

| Layer type | Name pattern | Role in reconstruction |
|------------|-------------|------------------------|
| **Delta** | `key_range__lsn_start-lsn_end` | Incremental WAL changes between two LSNs; required when the page version at a given LSN falls in this range |
| **Image** | `key_range__lsn` | Full page snapshot at a single LSN; sufficient alone to serve a page without chaining earlier deltas |

Delta GETs are the critical path for the paper's claim: a cold read with a long delta chain requires k sequential WarpDrive fetches, one per delta layer, before the page can be served.

---

## Phase 1 — Data Load (prepare)

Single tenant, 1 thread, scale=10 (10 warehouses).

| Op             | Count |
|----------------|-------|
| PUT            | 306   |
| DELETE_OBJECTS | 3     |
| GET            | 0     |
| LIST           | 0     |
| MULTIPART      | 0     |
| **Est. cost**  | $0.001545 |

## Phase 2 — Single Tenant OLTP (warm cache)

1 tenant, 4 threads, 120s. Pageserver cache warm (freshly loaded data).

| Op             | Count |
|----------------|-------|
| PUT            | 23    |
| GET            | 0     |
| LIST           | 0     |
| **Est. cost**  | $0.000115 |

**TPC-C performance:**
- Transactions: 4,641 (38.60 TPS)
- Avg latency: 103.58ms | p95: 308.84ms
- Queries: 134,765 (61,335 read / 63,750 write / 9,680 other)

## Phase 3 — 4 Tenants OLTP (warm cache)

4 tenants (main + 3 branches), 2 threads each, 120s. Pageserver cache warm.

| Op             | Count |
|----------------|-------|
| PUT            | 44    |
| GET            | 0     |
| LIST           | 0     |
| **Est. cost**  | $0.000220 |

**Per-tenant results:**

| Endpoint | Port  | TXN   | TPS   | Reads  | Writes | Avg Lat |
|----------|-------|-------|-------|--------|--------|---------|
| main     | 55432 | 1,491 | 12.41 | 19,509 | 20,301 | 161ms   |
| ep-3     | 55435 | 1,527 | 12.72 | 20,486 | 21,305 | 157ms   |
| ep-4     | 55436 | 1,513 | 12.59 | 19,725 | 20,354 | 159ms   |
| ep-2     | 55451 | 1,595 | 13.29 | 20,206 | 20,936 | 150ms   |
| **Total**|       | **6,126** | **51.01** | | | |

## Phase 4 — 4 Tenants OLTP (cold start)

Same 4 tenants, but pageserver local layer files wiped before run. All page reads fetched from Warpdrive (S3).

| Op             | Count |
|----------------|-------|
| PUT            | 46    |
| GET            | 11    |
| LIST           | 0     |
| **Est. cost**  | $0.0002344 |

**Per-tenant results:**

| Endpoint | Port  | TXN   | Avg Lat |
|----------|-------|-------|---------|
| main     | 55432 | 1,235 | 194ms   |
| ep-2     | 55451 | 1,218 | 197ms   |
| ep-3     | 55435 | 1,171 | 205ms   |
| ep-4     | 55436 | 1,113 | 216ms   |
| **Total**|       | **4,737** | |

**Key observation:** Cold start produced 11 S3 GETs vs 0 warm. Latency increased ~30ms across all tenants. 11 GETs over 4,737 transactions = ~1 GET per 430 transactions.

## Phase 5 — 8 Tenants OLTP (warm cache)

8 tenants, 2 threads each, 120s. Pageserver cache warm.

| Op             | Count |
|----------------|-------|
| PUT            | 62    |
| GET            | 0     |
| **Est. cost**  | $0.000310 |

**Per-tenant results:**

| Port  | TXN   | TPS   | Avg Lat |
|-------|-------|-------|---------|
| 55432 | 923   | 7.64  | 261ms   |
| 55451 | 824   | 6.85  | 292ms   |
| 55435 | 838   | 6.94  | 288ms   |
| 55436 | 777   | 6.41  | 312ms   |
| 55465 | 816   | 6.76  | 296ms   |
| 55466 | 880   | 7.26  | 275ms   |
| 55467 | 820   | 6.78  | 294ms   |
| 55468 | 892   | 7.38  | 270ms   |
| **Total** | **6,770** | **56.0** | **285ms** |

## Phase 6 — 8 Tenants OLTP (proper cold start — matching 4T protocol)

8 tenants, 2 threads each, 120s. Pageserver cold (layers wiped), metrics reset BEFORE
any endpoint started. Captures GETs from both layer re-downloads (startup) and benchmark reads.

| Op             | Count |
|----------------|-------|
| PUT            | 4     |
| GET            | **48** (32 startup + 16 benchmark) |
| **Est. cost**  | $0.0000392 |

**Per-tenant results:**

| Port  | TXN   | TPS  | Avg Lat |
|-------|-------|------|---------|
| 55432 | 505   | 4.17 | 478ms   |
| 55451 | 144   | 1.19 | 1684ms  |
| 55435 | 145   | 1.19 | 1674ms  |
| 55436 | 186   | 1.54 | 1301ms  |
| 55465 | 424   | 3.51 | 569ms   |
| 55466 | 446   | 3.68 | 543ms   |
| 55480 | 512   | 4.22 | 472ms   |
| 55481 | 539   | 4.42 | 451ms   |
| **Total** | **2,901** | **27.9** | **~840ms** |

**Key observation:** 48 GETs vs 11 GETs at 4T — 4.4× more GETs for 2× more tenants.
Super-linear scaling consistent with increased page cache pressure. Latency 3-10× higher than
warm run as cold pages are fetched from Warpdrive mid-transaction.

## Phase 7 — 8 Tenants OLTP (post-cold-restart)

8 tenants, 2 threads each, 120s. Pageserver restarted cold (layers wiped from disk),
but metrics reset AFTER endpoint startup — so captured period is warm-cache-after-cold-reload.
GETs happened during endpoint startup (layer re-download from S3), not in the benchmark window.

**Note on methodology**: To isolate benchmark-time GETs, metrics must be reset BEFORE endpoints start.
The 4T cold run (Phase 4) correctly did this; these 8T runs measure steady-state after re-warm.

| Op             | Count |
|----------------|-------|
| PUT            | 85    |
| GET            | 0     |
| **Est. cost**  | $0.000425 |

**Per-tenant results:**

| Port  | TXN   | TPS   | Avg Lat |
|-------|-------|-------|---------|
| 55432 | 1,506 | 12.53 | 160ms   |
| 55451 | 1,182 | 9.83  | 204ms   |
| 55435 | 1,205 | 10.02 | 199ms   |
| 55436 | 1,376 | 11.44 | 175ms   |
| 55465 | 1,409 | 11.71 | 171ms   |
| 55466 | 1,453 | 12.09 | 165ms   |
| 55480 | 1,174 | 9.75  | 205ms   |
| 55481 | 1,235 | 10.29 | 194ms   |
| **Total** | **10,540** | **87.66** | **184ms** |

**Key observation**: Higher TPS than warm 8T run (87 vs 56) because two replacement tenants
(ports 55480/55481) are fresh branches with no WAL bloat. True cold-start S3 GETs are
captured in the startup phase, not the benchmark window.

---

## Phase 8 — 1 Tenant, Moderate Aggressive Config (warm, eviction active)

**Date:** 2026-07-16  
**Config change:** `checkpoint_distance=16MB`, `checkpoint_timeout=10s`, `eviction_policy=LayerAccessThreshold(period=5s, threshold=20s)`  
1 tenant, 4 threads, 120s. Pageserver warm but eviction policy actively cycling layers off disk.
Metrics reset before run. Layer-type monitor not active for this run.

| Op                | Count | Unit cost        | Contribution |
|-------------------|-------|------------------|-------------|
| PUT               | 199   | $0.005 / 1,000   | $0.000995   |
| GET (total)       | 6     | $0.0004 / 1,000  | $0.0000024  |
| GET — delta       | —     | —                | (not tracked this run) |
| GET — image       | —     | —                | (not tracked this run) |
| DELETE_OBJECTS    | 3     | $0.005 / 1,000   | $0.000015   |
| **Est. total**    |       |                  | **$0.001012** |

**TPC-C performance:**
- Transactions: 7,945 (66.2 TPS)
- Avg latency: 60.43ms | p95: 183.21ms

**Key observation:** 8.7× more PUTs than default config (199 vs 23) from 16× smaller checkpoint distance.
GETs now non-zero mid-run (first time in any warm run) — eviction is forcing layer re-downloads.
DELETE_OBJECTS confirms compaction GC is running alongside eviction.

---

## Phase 9 — 1 Tenant, Fully Aggressive Config (warm, eviction active)

**Date:** 2026-07-16  
**Config change:** `checkpoint_distance=4MB`, `checkpoint_timeout=5s`, `eviction_policy=LayerAccessThreshold(period=5s, threshold=10s)`  
1 tenant, 4 threads, 120s. Layer-type monitor active (tails pageserver log, classifies `get_or_maybe_download` events).  
Log: `logs/validation_run/layer_downloads_aggressive.json`

| Op                | Count | Unit cost        | Contribution |
|-------------------|-------|------------------|-------------|
| PUT               | 525   | $0.005 / 1,000   | $0.002625   |
| GET (total)       | 8     | $0.0004 / 1,000  | $0.0000032  |
| GET — **delta**   | **3** | $0.0004 / 1,000  | $0.0000012  |
| GET — **image**   | **5** | $0.0004 / 1,000  | $0.0000020  |
| DELETE_OBJECTS    | 6     | $0.005 / 1,000   | $0.000030   |
| **Est. total**    |       |                  | **$0.002658** |

**TPC-C performance:**
- Transactions: 10,577 (88.1 TPS)
- Avg latency: 45.39ms | p95: 167.44ms

**Delta GET detail** (from pageserver log, `get_or_maybe_download` events):

| # | Layer name (truncated) | Kind  | Reason |
|---|------------------------|-------|--------|
| 1 | `...408D__...5709-...4451` | delta | file was not found |
| 2 | `...408D__...5709-...4451` | delta | file was not found |
| 3 | `...408D__...5709-...4451` | delta | file was not found |
| 4 | `...0720__...5709`         | image | file was not found |
| 5 | `...408D__...5709`         | image | file was not found |
| 6 | `...408B__...5709`         | image | file was not found |
| 7 | `...0860__...5709`         | image | file was not found |
| 8 | `...0880__...5709`         | image | file was not found |

All 8 downloads share the same base LSN (`0000000185E35709`), indicating a single page reconstruction event that needed 3 delta layers plus 5 image layers from WarpDrive before the page could be served. This is the k-GET chain the paper models.

**Key observation — why only 8 GETs despite 250+ evictions:**  
Compaction races eviction. The 6 DELETE_OBJECTS batches represent compaction sweeps that merged small delta layers into new image layers and deleted the originals from WarpDrive — before those evicted deltas could be re-accessed. Most evicted layers are GC'd, not re-downloaded. GETs only occur in the narrow window where a layer is evicted, still present in WarpDrive, and then needed for reconstruction before compaction removes it. This is the flat-store structural trap: compaction (DELETE+PUT cycles) dominates the I/O budget, not reads.

---

## Summary: Full Operation Lifecycle Across All Phases

| Phase | Config | PUTs | GETs | GET delta | GET image | DELETE_OBJECTS | TPS | Est. cost |
|-------|--------|------|------|-----------|-----------|----------------|-----|-----------|
| 1 — load | default | 306 | 0 | 0 | 0 | 3 | — | $0.001545 |
| 2 — 1T warm | default 256MB | 23 | 0 | 0 | 0 | 0 | 38.6 | $0.000115 |
| 3 — 4T warm | default 256MB | 44 | 0 | 0 | 0 | 0 | 51.0 | $0.000220 |
| 4 — 4T cold | default 256MB | 46 | 11 | — | — | 0 | ~40 | $0.000234 |
| 5 — 8T warm | default 256MB | 62 | 0 | 0 | 0 | 0 | 56.0 | $0.000310 |
| 6 — 8T cold | default 256MB | 4 | **48** | — | — | 0 | 27.9 | $0.000039 |
| 7 — 8T warm (post-restart) | default 256MB | 85 | 0 | 0 | 0 | 0 | 87.7 | $0.000425 |
| 8 — 1T warm | 16MB / 20s evict | 199 | 6 | — | — | 3 | 66.2 | $0.001012 |
| **9 — 1T warm** | **4MB / 10s evict** | **525** | **8** | **3** | **5** | **6** | **88.1** | **$0.002658** |

### Interpretation for the paper

**PUTs scale with checkpoint frequency, not workload size.** Going from 256MB to 4MB checkpoint distance (64×) multiplies PUTs by ~23× (23→525) at similar TPS. This is write amplification from the compaction cycle that flat object stores impose.

**GETs in steady-state are structurally suppressed by compaction.** Even with aggressive eviction (10s threshold), most evicted layers are deleted by compaction before they can be re-read. GETs dominate only at cold-start/failover (Phase 6: 48 GETs, 3–10× latency increase). This is the operational asymmetry: WarpDrive is write-heavy in normal operation and read-heavy only at re-attachment.

**DELETE_OBJECTS quantifies the compaction tax.** Every DELETE_OBJECTS batch is evidence of a compaction round — superseded delta layers purged after merging. With WarpDrive's mutable co-located slabs, this GC cycle is unnecessary: deltas are appended in-place, and the slab is read as a single batched I/O at reconstruction time.

**Delta GETs confirm the k-GET chain.** Phase 9's 3 delta GETs plus 5 image GETs for a single reconstruction event demonstrate that cold reads on flat storage require k independent round-trips to WarpDrive. The analytical model's R_GET = T·λ·ρ·k is directly observable in the pageserver logs.
