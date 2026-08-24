# Known Issues — found during GCP benchmark packaging (2026-08-22/23)

Found while setting up the Neon + WarpDrive/MinIO scaling benchmark on GCP
(`docs/benchmarks/init_cluster.sh`, `docs/benchmarks/copy_bucket_nonchunked.sh`).
Two of these are workarounds already applied in the scripts; one is an actual
WarpDrive product bug that should be fixed properly later.

## 1. WarpDrive doesn't decode `aws-chunked` streaming uploads (real bug)

**Where:** `warpdrive/server/src/s3/handlers/object.rs`, `put_object` handler.

**What happens:** Lines 143–152 strip the literal string `"aws-chunked"` out of
the `Content-Encoding` header value before storing it as object metadata — but
that only fixes what WarpDrive later *claims* about the object. The actual
body-read loop (lines 168–193, `while let Some(chunk_result) = payload.next().await`)
writes the raw HTTP payload bytes straight to storage with no awareness that
AWS SigV4 streaming ("`aws-chunked`") uploads wrap the real payload in
chunk-size/chunk-signature framing:

```
<hex-chunk-size>;chunk-signature=<sig>\r\n<chunk-bytes>\r\n...0;chunk-signature=<sig>\r\n\r\n
```

The framing bytes end up stored as part of the object content instead of
being stripped, corrupting the object.

**Trigger:** Any S3 client that uses chunked/streaming signed uploads for
`PutObject` — confirmed with `mc mirror` and (by extension) `aws s3 cp`/`aws s3 sync`.
A 57-byte JSON file came back as 230 bytes with visible
`39;chunk-signature=...` framing wrapped around the real content.

**Not triggered by:** plain single-shot PUT (`aws s3api put-object`, which
sends a known `Content-Length` body, no chunking) — round-trips byte-identical,
confirmed with ETag match. This is also almost certainly the code path Neon's
pageserver's own Rust S3 client uses for its checkpoint/layer-file uploads,
which is presumably why this was never caught in earlier local WarpDrive
benchmark runs — the actual benchmark write path is not affected.

**Workaround used in this session:** `docs/benchmarks/copy_bucket_nonchunked.sh`
copies objects via `aws s3api get-object` + `put-object` per key instead of
`mc mirror`, to move data between MinIO and WarpDrive without corruption.

**Real fix needed:** the `put_object` handler must detect
`Content-Encoding: aws-chunked` (or `x-amz-content-sha256: STREAMING-*`) and
actually parse/strip the chunk framing before writing bytes to storage, not
just adjust the header it reports back.

## 2. Safekeeper `remote_storage` TOML got malformed — in an ad-hoc command, not the script

**Where:** a throwaway SSH heredoc I improvised live while re-initializing
the cluster to test WarpDrive `tenant import` — **not**
`docs/benchmarks/init_cluster.sh` itself.

**What happened:** that ad-hoc heredoc mixed a raw Python string (`r"..."`)
with `\"` escapes intended to produce literal double-quotes. Raw strings
don't process that escape, so a literal backslash landed in `.neon/config`'s
safekeeper block: `remote_storage = \"{endpoint="http://...`. That's invalid
TOML.

**Correction:** I initially assumed this bug also lived in
`init_cluster.sh`'s own config-patching logic and noted it as a TODO there.
Re-checked by extracting that exact code and running it standalone against a
test file — it produces correct output
(`remote_storage = "{endpoint='http://...', ...}"`, matching the working
pattern from `docs/compatibility_tests/neon.md`). **`init_cluster.sh` does
not have this bug**; it was specific to the manual command I typed at the
terminal when re-patching an already-initialized cluster by hand. No fix
needed in the script for this one.

## 3. Pageserver's AWS SDK hangs probing EC2 Instance Metadata Service on GCP

**Where:** environment/deployment issue, not a code bug.

**What happens:** pageserver's AWS credential provider chain tries the EC2
IMDS provider before falling back to env-var credentials. On a GCP VM there's
no real IMDS endpoint at that path — GCP's own metadata server responds with
an unrelated 405, and the SDK's handling of that was slow (>2 min, still
retrying with backoff when killed) rather than failing fast to the next
provider. Hit specifically during `neon_local tenant import`'s
`scan_remote_storage` call.

**Fix applied:** set `AWS_EC2_METADATA_DISABLED=true` in the environment
`neon_local start` launches pageserver with. Needs to be set before
`neon_local start`, not just before the CLI command that triggers the S3
call — pageserver is a long-running process, so the env var has to be present
when *it* starts, not just in the shell running `neon_local tenant import`.

**Status:** added `AWS_EC2_METADATA_DISABLED=true` as a standard exported env
var in `init_cluster.sh` (alongside the other AWS_* exports, before
`neon_local start`). Any other script that calls `neon_local start` on GCP
should export it too.

## 4. Endpoint HTTP port collisions (real bug, invalidated first MinIO sweep)

**Where:** our own port-derivation formula, used both in `init_cluster.sh`
and in the manual endpoint-creation commands: `internal_http_port =
pg_port-2`, `external_http_port = pg_port-1`.

**What happened:** the 16-endpoint roster has several `pg_port`s spaced only
1-2 apart (e.g. `ep-5`=55465, `ep-6`=55466). With that formula, `ep-6`'s
`external_http_port` (55465) collides directly with `ep-5`'s `pg_port`
(55465), and `ep-6`'s `internal_http_port` (55464) collides with `ep-5`'s
`external_http_port` (55464). Checked programmatically: **10 of 16
endpoints** had at least one colliding port (`ep-6`, `ep-b`, `ep-f`, `ep-g`,
`ep-h`, `ep-l`, `ep-m`, `ep-n`, `ep-o`, `ep-p`).

**Impact — not uniform, which made it confusing:**
- `external_http_port` collisions caused the endpoint's `/status` health
  check (used by `neon_local endpoint start`) to hit the wrong service and
  report "failed" / "connection closed before message completed" — but the
  underlying Postgres process (bound to its own correct `pg_port`) could
  still be running fine underneath. `ep-6` "failed" to start yet still
  produced 4.1 TPS during the actual sysbench run.
- `internal_http_port` collisions were more fatal — `compute_ctl` needs that
  port for its own control duties before Postgres starts, so a bind conflict
  there killed the process outright (`ep-l`: "Postgres exited unexpectedly
  with code 1", confirmed dead — 0.0 TPS).

Net effect: the first full MinIO T=1/4/8/16 sweep (2026-08-23 06:36-07:06
UTC) is **not valid** for T=4/8/16 — each includes at least one colliding
endpoint, and T=16's per-endpoint latencies show a suspicious uniform
~31650ms cluster across many endpoints, consistent with a shared
failure/retry pattern rather than genuine independent TPC-C variance. T=1
(just `main`) is unaffected.

**Fix applied:** reassigned all 16 endpoints (both the live `.neon/endpoints/*/endpoint.json`
files and `init_cluster.sh`'s formula) to a completely disjoint port range:
`internal_http_port = 60000 + index`, `external_http_port = 61000 + index`,
guaranteed never to overlap the `pg_port` range (55000s) or each other.
Re-ran the sweep after fixing.

## Research points — MinIO vs. WarpDrive fairness (not bugs, revisit before drawing conclusions)

Checked whether MinIO is running in some distributed/erasure-coded mode that
would make the baseline comparison unfair. `mc admin info` confirms it's
genuinely single-node/single-drive standalone: `Drives: 1/1`, `Pool: 1`,
`Erasure stripe size: 1`, `Erasure sets: 1`, `EC:0` — no erasure coding, no
replication, no distributed consensus. Nothing to disable there; already the
minimal config.

Two real, structural differences did turn up in `mc admin config get`,
worth understanding before treating MinIO-vs-WarpDrive numbers as a clean
apples-to-apples comparison:

1. **`api odirect=on`** (MinIO default) — MinIO's storage layer uses
   `O_DIRECT` I/O, bypassing the OS page cache entirely for reads/writes. If
   WarpDrive instead uses normal buffered I/O (page-cache-backed), that's a
   genuine asymmetry in how the two backends interact with disk, independent
   of any application-level design difference. **TODO:** check what I/O mode
   WarpDrive's `LocalXFSSlabStore` actually uses; if it's buffered, consider
   a second MinIO run with `mc admin config set minio api odirect=off` for a
   more controlled comparison point (note: `odirect=on` is MinIO's
   recommended production setting, so turning it off is a deliberate
   benchmark-fairness choice, not "more realistic").
2. **`xl.meta` sidecar file per object** — MinIO's object format always
   writes a companion metadata file alongside object data (shared code path
   with the distributed/EC case, present even in single-drive mode) — an
   extra small file write + fsync per PUT that a simpler flat-file store may
   not incur. Not configurable away; inherent to MinIO's on-disk format.
3. **Background scanner** (`scanner speed=default`) — periodic
   usage/capacity crawling. Tunable (`slow`/`slowest`) if it turns out to add
   measurable jitter, though `storage-backend` was observed near-idle
   throughout setup so this is likely a non-factor.

Also unremarkable / already off, checked and ruled out: compression,
versioning, object locking, replication, gzip.

## Research point — TPS collapse at T=8/16 is a concurrency-limit ceiling, not resource saturation

Diagnosing why MinIO-baseline TPS drops (23.1 @ T=4 → 21.9 @ T=8 → 18.1 @
T=16) and avg latency explodes (690ms → 20.6s → 41.4s) despite neither node
showing resource pressure (`neon-compute` peaks at 37.6% CPU across 16 cores;
`storage-backend` peaks at 12.6% CPU across 8 cores at T=16 — see the full
results table from the 2026-08-23 07:08-07:25 UTC clean MinIO sweep).

**Checked pageserver's own S3 client metrics** (`remote_storage_s3_request_seconds`,
queried live off `/metrics` right after the T=16 run, scoped to that window
since pageserver restarts fresh each T):
- `get_object`: 1,514 requests, avg **993ms**
- `put_object`: 1,517 requests, avg **52ms**
- `delete_object` / `list_objects`: <20ms avg

**Cross-checked against MinIO's own server-side TTFB histogram** (via
`mc admin prometheus generate` for a bearer token, then
`/minio/v2/metrics/cluster`): 92.5% of `getobject` requests complete
**≤50ms** server-side; virtually all complete within 500ms.

**Server says fast, client says slow — classic client-side queuing
signature.** Found the likely mechanism in `neon-src/libs/remote_storage/src/lib.rs`:

```rust
pub const DEFAULT_REMOTE_STORAGE_S3_CONCURRENCY_LIMIT: usize = 100;
```

This is a **global semaphore** in pageserver's S3 client (one per pageserver
process, shared across all tenants/timelines) — only 100 GET/PUT requests can
be in flight to the object store at once. Not overridden anywhere in our
`pageserver.toml`, so we're on this default. At T=16, up to 64 concurrent
Postgres connections (16 endpoints × 4 sysbench threads) hitting a
freshly-wiped cache can easily generate far more than 100 simultaneous GET
needs; everything past the limit queues *inside pageserver* waiting for a
free slot before it ever reaches MinIO. The math checks out: a request
queued behind ~1,400 others at roughly 50ms/batch-of-100 waits ~700ms before
its own ~50ms dispatch — landing right around the observed 993ms average.

**Not yet confirmed, just well-triangulated** — haven't found a direct
"queue depth" metric to prove it beyond the arithmetic + the MinIO/pageserver
cross-check. **TODO before drawing conclusions:** bump `concurrency_limit` in
the `remote_storage` config (e.g. to 500-1000) and rerun T=8/T=16 — if TPS
scales meaningfully better, that confirms this was the real ceiling, not
disk/network/CPU on either node. Worth doing for both MinIO and WarpDrive
runs so the comparison isn't confounded by an artificial software throttle
neither backend actually controls.
