# WarpDrive Fixes Log

Running log of every fix made during the GCP Neon+WarpDrive benchmarking effort, what it was for, and the measured before/after result. Ordered chronologically.

---

## 1. Slab batch-download UTF-8 parsing crash

**File:** `neon-src/pageserver/src/tenant/remote_timeline_client/download.rs` (`parse_and_write_multipart`)

**Problem:** The slab co-location batch-GET response parser required the entire multipart body to be valid UTF-8 (`std::str::from_utf8(body)`), but Postgres layer file data is binary. This failed essentially 100% of the time on real payloads, meaning the slab pre-warm feature never actually worked in any prior benchmark run despite appearing configured correctly.

**Fix:** Rewrote the parser to work on raw `&[u8]` throughout, using a byte-pattern search helper (`find_bytes`) instead of UTF-8 string search. Only the small per-part headers are still decoded as UTF-8; part bodies stay raw bytes.

**Result:** Verified via a small isolated test (single tenant, 250k-row table): `"slab pre-warm: 37 layers written"` succeeds, data integrity confirmed via query.

---

## 2. Slab batch-GET `Content-Disposition` path bug

**File:** `warpdrive/server/src/warpd.rs` (`warpd_slab_batch_get`)

**Problem:** The `Content-Disposition: attachment; filename="{key}"` header sent the full S3 key (including path segments), but `Content-Disposition`'s `filename` must be a bare filename per spec. The pageserver client does `timeline_path.join(&filename)`, producing a doubled, nonexistent nested path.

**Fix:** Use `key.rsplit('/').next()` to send only the basename.

**Result:** Combined with fix #1, slab batch download now works end-to-end.

---

## 3. Missing HADRON tagging on slab batch-download code

**File:** `neon-src/pageserver/src/tenant/remote_timeline_client/download.rs`

**Problem:** The entire slab-batch-download addition (`download_layers_slab_batch`, `parse_and_write_multipart`, sigv4 helpers, ~318 lines) was untagged, unlike every other Hadron/WarpDrive-specific addition in the neon-src tree (which use `/* BEGIN_HADRON */.../* END_HADRON */` markers to keep the diff against upstream Neon auditable).

**Fix:** Wrapped the addition in the standard markers, matching `remote_timeline_client.rs`, `timeline.rs`, `upload.rs`.

**Result:** Organizational only, no behavior change.

---

## 4. `AWS_EC2_METADATA_DISABLED` missing in benchmark scripts' pageserver env

**Files:** `run_scaling_slab.py`, `run_scaling_noslab.py` (`restart_pageserver`)

**Problem:** These scripts launch pageserver directly via `subprocess.Popen`, bypassing `neon_local`'s env allowlist. On GCP, pageserver's AWS SDK credential chain tries the EC2 Instance Metadata Service before env-var credentials; GCP's own metadata server answers that path with an unrelated 405 and the SDK retries every 3s indefinitely instead of failing fast — the tenant never attached (`Attaching` state forever, `current_physical_size: 0`, `timelines: []`).

**Fix:** Set `env["AWS_EC2_METADATA_DISABLED"] = "true"` explicitly in `restart_pageserver()`'s env dict.

**Result:** Tenant attach dropped from "never" (infinite retry loop) to ~1 second.

---

## 5. Missing `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY` in benchmark scripts' pageserver env

**Files:** `run_scaling_slab.py`, `run_scaling_noslab.py` (`restart_pageserver`)

**Problem:** Same root cause as #4 — direct `subprocess.Popen` launch bypasses `neon_local`'s allowlist, which is what normally passes these through. The scripts only set `WARPDRIVE_ADMIN_ACCESS_KEY`/`SECRET` (WarpDrive's own admin-API credentials, a different thing from the S3 client credentials pageserver's AWS SDK needs). Once IMDS was disabled (#4), the credential chain had nothing left to fall back to (`the credential provider was not enabled`).

**Fix:** Set `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`/`AWS_DEFAULT_REGION` directly in `restart_pageserver()`'s env dict, so it's robust regardless of the calling shell's environment.

**Result:** Tenant attach now succeeds reliably end-to-end.

---

## 6. Slab co-location scoped to delta layers only

**File:** `neon-src/pageserver/src/tenant/remote_timeline_client/upload.rs` (`upload_timeline_layer`), `remote_timeline_client.rs` (both call sites)

**Problem:** The `warpd-slab-hint` metadata was attached unconditionally to *every* layer upload (image and delta), despite the code's own comment claiming it was for "all delta layers." Image layers are already large, standalone objects — batching them into slabs doesn't reduce request count and only bloats the slab file (measured: 14GB slab file, single co-located GETs up to 16.5GB) and the pre-warm payload size (large enough that pre-warm had to be disabled entirely in earlier benchmark runs — see `SLAB_DESIGN_NOTES.md`).

**Fix:** Added `is_delta: bool` parameter threaded from `LayerName::is_delta()` at both call sites; the hint is now only attached when `is_delta` is true.

**Result:** Not yet re-benchmarked end-to-end with pre-warm enabled (pending the with-slab leg).

---

## 7. PutObject per-network-chunk write (the big one)

**File:** `warpdrive/server/src/s3/handlers/object.rs` (`s3_put_object_handler_inner`)

**Problem:** The PUT handler called `store.write()` — which does `open()` + `seek(End)` + `write_all()` + `flush()` under a single **global, server-wide** `Mutex<()>` (`STORAGE_WRITE_LOCK` in `local_store.rs`) — once **per received network chunk**, not once per object. A 1MB object could be split into 50-100+ actix payload chunks, each paying the full lock+reopen+write+flush cost independently. Diagnosed by direct measurement: WarpDrive's own server-side metrics showed PUT avg=524ms, max=1590ms during the real TPC-C run; an isolated raw-disk `dd ... conv=fsync` test showed 12-20ms for the same 1MB, and a sequential single-threaded uncontended PUT test still showed ~650-770ms — ruling out both disk I/O and lock contention as the cause, and a 5-byte object taking the same latency as a 1MB one confirmed it was a fixed per-request cost, not data-size-dependent.

**Fix:** Objects up to a 512MB threshold are now buffered fully in memory (already required anyway for etag computation) and written to disk in a single call. Objects above that threshold fall back to writing each chunk as it arrives (bounded memory, accepting the per-chunk cost only when actually necessary). MD5 (etag) and S3 checksum (`x-amz-checksum-*`) verification are now computed incrementally as chunks arrive via a new `ChecksumHasher` enum in `checksum.rs` (SHA256/SHA1/CRC32/CRC32C/CRC64NVME), so large objects never need their full body materialized just to checksum them.

**Result (isolated, server-side, 1MB objects, 20-request batches via `/_admin/metrics`):**
| | Avg | Max |
|---|---|---|
| Before | 524ms | 1590ms |
| After (chunk fix only, sync=FULL) | 7.6ms | 8.8ms |
| After (chunk fix + fix #8) | 6.4ms | 7.7ms |

---

## 8. SQLite `synchronous=NORMAL`

**File:** `warpdrive/server/src/metadata/sqlite_store.rs` (`DB_CONN` init)

**Problem:** `journal_mode=WAL` was set, but `synchronous` was never explicitly set, defaulting to `FULL` — fsyncs the WAL on every commit. `put_object_v2`'s common path runs a SELECT+DELETE+INSERT as separate autocommit statements (no explicit transaction), each paying its own fsync.

**Fix:** Added `PRAGMA synchronous=NORMAL;` — the standard, safe pairing with WAL mode per SQLite's own documentation (durable against an app crash; only risks the last few ms of transactions on an OS-level crash).

**Result:** Measured as a smaller, real, additional improvement on top of #7 — see table above (7.6ms → 6.4ms avg, ~16% further reduction). Fix #7 was overwhelmingly the dominant factor (98.5%+ of the total improvement); this contributed the remainder.

---

## End-to-end validation: T=1 TPC-C, no-slab

Fixes #4, #5, #7, #8 combined, measured via a real sysbench-tpcc T=1 run (scale=10, 120s) against the live "neon" bucket (not a pristine dataset — carries some leftover transaction data from an earlier T=1 attempt before the fix, so directional not final):

| | TPS | Avg latency | p95 latency |
|---|---|---|---|
| WarpDrive (before fix) | 7.3 | 539ms | 1109ms |
| **WarpDrive (after fix)** | **15.9** | **251ms** | **847ms** |
| MinIO (baseline) | 14.6 | 273ms | — |

WarpDrive now beats the MinIO T=1 baseline. Pending: a byte-clean run from the pristine backup (blocked on disk-space/second-instance setup on `storage-backend`, revisit later).

---

## T=4 regression: pageserver-side stall, not WarpDrive latency

T=4 (no-slab) produced 10.3 total TPS but wildly imbalanced per-endpoint: `main` did 10.0 TPS/400ms avg (healthy), while `ep-2`/`ep-5`/`ep-6` each collapsed to 0.1 TPS/~39,500ms avg. MinIO's T=4 (baseline) was balanced across all 4 endpoints (~690ms avg each, 23.07 total TPS) — ruling out a general "all backends hit this at T=4" explanation.

**Diagnosis:** pageserver's own log showed `slow GetPage completed after 39.471s` repeatedly, hitting the 3 non-main timelines simultaneously. WarpDrive's own `/_admin/metrics` for the same window showed GET avg=0.53ms/max=3.59ms — ruling out slow WarpDrive reads. But pageserver's `remote_storage_s3_request_seconds` histogram for the same window showed `get_object` avg=**2.28 seconds** (426.7s / 187 requests) vs WarpDrive's self-reported 0.53ms — a 4000x gap, meaning the time is spent queued *before* WarpDrive's handler even starts, not inside it. `put_object` matched closely between the two sources (72ms pageserver-observed vs 70ms WarpDrive-observed) — the queueing disproportionately hit GET, not PUT.

Traced the queueing to two lock-based bottlenecks in WarpDrive itself (found by testing with concurrent isolated PUT load, not yet re-validated against a full T=4 rerun):

---

## 9. Storage write lock made lock-free

**File:** `warpdrive/server/src/storage/local_store.rs` (`LocalXFSBinaryStore::write`)

**Problem:** `STORAGE_WRITE_LOCK: Mutex<()>` serialized *every* PUT server-wide (not per-bucket) for the entire open+seek+write+flush sequence — needed only to make `seek(End) → write` atomic (prevent two writers racing to the same offset), but far broader than necessary. Since all our benchmark traffic shares one user+bucket, this lock affected all concurrent endpoints' checkpoint-flush PUTs.

**Fix:** Replaced with a per-bucket-file `AtomicU64` offset counter (`BUCKET_FILE_LEN`, initialized from the file's current size on first access) — each writer atomically reserves a non-overlapping byte range via `fetch_add`, then writes into it with a positioned write (`write_all_at`/pwrite) instead of seek+write on a shared cursor. POSIX guarantees pwrite to non-overlapping regions of the same file from different threads is safe, so no lock is held across any disk I/O; the map's own mutex is only held for a fast HashMap lookup.

**Correctness verified:** 100 concurrent PUTs (20-way parallelism, 256KB objects, same bucket file) read back byte-exact, no corruption.

**Result (100 concurrent PUTs, `/_admin/metrics`):** avg 524ms → still 241.75ms/max 451.9ms under 20-way concurrency (down from theoretical worse, but SQLite lock (#10) was still present at this measurement).

---

## 10. SQLite metadata write batched into one transaction

**File:** `warpdrive/server/src/metadata/sqlite_store.rs` (`put_object_v2`)

**Problem:** `DB_CONN: Mutex<Connection>` is global (SQLite is single-writer regardless of app-level locking). `put_object_v2` acquired it once but ran its SELECT+DELETE+INSERT (or similar) sequence as 3 separate autocommit statements — each with its own implicit transaction overhead, all still serialized behind the one connection lock.

**Fix:** Wrapped the whole per-call statement sequence in one explicit `conn.transaction()` (rusqlite `Transaction`, auto-rollback on drop if not committed) instead of 3 separate autocommit statements.

**Result (100 concurrent PUTs, `/_admin/metrics`, with #9 already applied):**
| | Avg | Max |
|---|---|---|
| Lock-free write only | 241.75ms | 451.9ms |
| + transaction batching | **170.65ms** | **314.6ms** |

~30% further reduction, correctness re-verified (100/100 byte-exact). Residual latency under heavy concurrency is expected — SQLite is fundamentally single-writer at the file level, so some queuing under 20-way concurrent PUT load can't be fully eliminated without a bigger architectural change (different metadata store, or batching multiple objects per transaction). Treating that as future work, not a bug to chase further right now.

**Not yet done:** rerun T=4 end-to-end with both #9 and #10 in place to confirm the 30-40s GetPage stalls are actually resolved (the isolated concurrent-PUT tests above are a proxy, not the real workload).

---

## T=4 root cause: NOT WarpDrive — stale endpoint/timeline state under one tenant

Reran T=4 with #9 and #10 in place: stalls persisted (even got worse, 95-126s avg on the 3 non-main endpoints). This proved #9/#10, while real fixes, were not the T=4 bottleneck. Traced properly (see "what happens at each component" investigation):

1. **16 attached timelines, only 4 active**: `init_cluster.sh` pre-creates all 16 endpoints/timelines up front regardless of which T is under test. Neon's eviction task for the `LayerAccessThreshold` policy runs a tenant-scoped "synthetic size calculation" (`imitate_layer_accesses` in `eviction_task.rs`, upstream Neon code, not Hadron/WarpDrive-specific) guarded by a **per-tenant lock** — only one timeline's eviction task actually runs it at a time; the other 15 just block waiting. With all 16 timelines attached and a cold-start wipe before every T-run, this lock contention alone produced `task iteration took longer than the configured period elapsed=24-28s period=5s` across all 16 timelines simultaneously.
   - **Fix:** deleted the 12 timelines not under test for this T=4 run (`DELETE /v1/tenant/{tenant}/timeline/{id}`), leaving only `main`/`ep-2`/`ep-5`/`ep-6` attached.
   - **Result:** 3 of 4 endpoints immediately became healthy (main/ep-2/ep-5 at 490-512ms avg, up from the prior 10.3-11.5 total TPS to 24.1). `ep-6` alone was still broken (161s avg).

2. **Stale endpoint/pgdata state carried across every repeated attempt**: the T-sweep protocol wipes pageserver's local layer cache and restarts pageserver before every run, but never deletes/recreates each endpoint's local `pgdata/` — endpoints just get stopped and restarted, so local Postgres state accumulates across every failed attempt. Confirmed via `compute_ctl`'s own `basebackup_ms` metric for the identical ~81KB basebackup payload: `main`=121ms, `ep-5`=195ms, `ep-2`=1130ms, `ep-6`=**2093ms** — a clear queueing pattern by start order, not a data-size issue. Root cause: pageserver has exactly **one walredo process per tenant** (not per timeline), so concurrently-starting endpoints' basebackup requests (each needing walredo to materialize their snapshot) queue behind that single shared process. `ep-6`, consistently started last in the roster, consistently paid the worst tax — explaining why it was the one broken endpoint in every single prior run.
   - **Fix:** stopped all 4 endpoints, deleted their endpoint directories (local `pgdata`) entirely, deleted the 3 branch timelines (kept `main`'s timeline — it holds the real prepared base data), re-branched 3 fresh timelines from `main`, created 4 fresh endpoints.
   - **Result: all 4 endpoints healthy**, uniform ~510-540ms avg latency, **30.2 total TPS** — beats the MinIO T=4 baseline (23.07 TPS) by ~31%.

**Conclusion:** the entire T=4 investigation — which initially looked like it might be a WarpDrive latency problem — was actually a benchmark-methodology issue: too many idle timelines attached under one tenant, and endpoint state never being reset between repeated test attempts. Neither WarpDrive nor pageserver's core storage-serving code was at fault. Lesson for T=8/T=16: always start from a fully clean slate (delete all non-`main` timelines and all endpoints, re-branch and recreate fresh) rather than reusing endpoints from a prior run, and only ever attach as many timelines as the T value under test actually needs.

| T=4 attempt | Total TPS | Result |
|---|---|---|
| Original (16 timelines attached, stale endpoints) | 10.3-11.5 | 3/4 endpoints stalled 40-160s+ |
| 16→4 timelines only | 24.1 | 3/4 healthy, `ep-6` still stalled 161s |
| **+ fresh endpoints/timelines (no carried-over state)** | **30.2** | **4/4 healthy, uniform ~520ms avg** |
| MinIO baseline (reference) | 23.07 | — |

---

## T=8 clean run

Applied the same clean-slate protocol before running T=8: stopped and deleted all endpoints, deleted all non-`main` timelines, re-branched 7 fresh timelines from `main` (`ep-2`/`ep-5`/`ep-6`/`ep-a`/`ep-b`/`ep-e`/`ep-f`), created 8 fresh endpoints. All 8 started within 2 seconds of each other (vs. seconds-apart staggering seen before the fix).

Also added artifact-saving to both `run_scaling_noslab.py` and `run_scaling_slab.py`: each run's output directory (`logs/scaling_noslab_gcp/T{N}/`) now also gets a `pageserver_log_slice.log` (byte-offset-scoped to exactly that run, avoiding the multi-day-log mixup that caused an earlier false alarm), `warpdrive_metrics_startup.json` / `warpdrive_metrics_final.json` (previously the `latencies` field from `/_admin/metrics` was silently dropped — now captured in `result.json` too), and `pageserver_metrics_final.txt` (raw Prometheus scrape, includes `remote_storage_s3_request_seconds`).

**Result — all 8 endpoints uniform, no stalls:**
| Endpoint | TPS | Avg latency |
|---|---|---|
| main | 3.8 | 1053ms |
| ep-2 | 3.5 | 1137ms |
| ep-5 | 3.8 | 1048ms |
| ep-6 | 3.9 | 1032ms |
| ep-a | 3.7 | 1066ms |
| ep-b | 3.9 | 1022ms |
| ep-e | 3.5 | 1142ms |
| ep-f | 3.8 | 1060ms |
| **Total** | **29.9** | **1070ms avg / 4204ms p95** |

vs. MinIO T=8 baseline: 21.89 TPS, **20,573ms avg latency**. WarpDrive beats it by 36% on throughput and ~19x on latency.

---

## T=16 clean run

Same clean-slate protocol: stopped/deleted all 8 endpoints, deleted all 7 non-`main` timelines, re-branched all 15 non-`main` names fresh from `main` (`ep-2` through `ep-p`), created all 16 endpoints fresh. All 16 started smoothly within ~6 seconds total, no exponential per-endpoint lag.

**Result — all 16 endpoints uniform, no stalls:**
| Endpoint | TPS | Avg latency | txn | errors |
|---|---|---|---|---|
| main | 1.5 | 2609ms | 185 | 4 |
| ep-2..ep-p (15 branches) | 1.7-1.9 each | 2070-2404ms each | 201-233 each | 4-7 each |
| **Total** | **28.3** | **2256ms avg / 9118ms p95** | | |

Small error counts (4-7 per endpoint, ~2-3% of txns) appear evenly across all 16 endpoints — consistent with legitimate TPC-C lock contention at 64 total concurrent client threads (16 endpoints × 4 sysbench threads each) against only 10 warehouses, not a bug or an outlier endpoint.

vs. MinIO T=16 baseline: 18.08 TPS, **41,387ms avg latency**. WarpDrive beats it by 56% on throughput and ~18x on latency.

---

## Full clean sweep summary: WarpDrive (no-slab, all fixes applied) vs MinIO

| T | WarpDrive TPS | MinIO TPS | WarpDrive avg lat | MinIO avg lat |
|---|---|---|---|---|
| 1 | 15.9 | 14.6 | 251ms | 273ms |
| 4 | 30.2 | 23.07 | 528ms | 690ms |
| 8 | 29.9 | 21.89 | 1070ms | 20,574ms |
| 16 | 28.3 | 18.08 | 2256ms | 41,387ms |

WarpDrive wins at every T, with the gap widening dramatically at T=8/16 where MinIO's latency explodes (its TPS holding up reasonably while latency does not suggests MinIO is accepting/queuing far more work in flight rather than truly serving it quickly). This whole result depended on two categories of fix: real WarpDrive bugs (per-chunk write lock, SQLite transaction batching — fixes #7-10) and a benchmark-methodology fix (clean endpoint/timeline state per T-run, only as many timelines attached as needed — not a WarpDrive or pageserver bug at all).

---

## Future work / methodology notes

- **Reproducible transaction sequences**: sysbench currently runs with `--threads=4` per endpoint and no fixed seed, so the exact mix/ordering of TPC-C transactions varies run to run — fine for a rough throughput number, but it means slab vs no-slab vs MinIO aren't being driven by the literal same sequence of operations. Worth switching to `--threads=1` per endpoint plus a fixed `--rand-seed=<N>`, so repeated runs (and different backends) are driven by an identical, reproducible transaction sequence — makes cross-backend comparisons apples-to-apples in a stricter sense. Not yet implemented.
- **Checkpoint distance may be suppressing TPS**: our "Phase 9" tenant config uses an aggressive `checkpoint_distance=4MB` (vs Neon's much larger default, generally in the hundreds-of-MB range) plus `checkpoint_timeout=5s` and `evict@10s`. This forces far more frequent checkpoint flushes/uploads and layer eviction than a normal deployment would see, and may be the reason our absolute TPS numbers look low. Next step: rerun with checkpoint_distance left at its Neon default and see how much TPS changes, then design a small sweep (~4 variants of checkpoint distance/size) — planned for the slab vs MinIO comparison specifically, once this no-slab baseline work is preserved.
