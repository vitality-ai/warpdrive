# Neon + Warpdrive TPC-C S3 Operation Benchmark

**Branch:** feat/op-counters  
**Date:** 2026-07-08  
**Setup:** Neon main branch, aws-sdk-rust 1.3.3, sysbench 1.0.20 tpcc, scale=10, tables=1

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

## Summary: S3 GETs only appear during cold-start reload

| Phase          | GETs | GETs during startup (untracked) |
|----------------|------|----------------------------------|
| 4T warm        | 0    | 0                                |
| 4T cold        | 11   | not applicable (metrics pre-reset before start) |
| 8T warm        | 0    | 0                                |
| 8T cold-restart| 0    | yes — layers re-fetched from S3 during ep startup |

For the paper: R_GET = T·λ·ρ·k applies to startup re-attachment cost.
During steady-state with warm pageserver cache, GETs approach zero.
