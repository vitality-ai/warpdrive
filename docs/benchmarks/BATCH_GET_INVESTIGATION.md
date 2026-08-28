# Batch-GET primitive investigation — naive client vs AnyBlob baseline

## Idea

Step 2 of the composability story started in `ANYBLOB_BASELINE.md`: that
document established what plain individual GET/PUT looks like on MinIO and
WarpDrive under AnyBlob's engineered client-side concurrency. This document
asks the actual question that matters — hit WarpDrive's server-side
batching + colocation primitive (`GET /_warpd/slab/{bucket}?hint=...`) with
a *trivial*, unsophisticated client (one HTTP request, no concurrency
tuning, no io_uring) and see what it takes to get that primitive competitive
with the client-engineering-heavy alternative.

Two different mechanisms, kept distinct throughout:
- **AnyBlob's "batching"** = client-side request concurrency (many
  overlapped individual round trips, still N of them).
- **WarpDrive's slab primitive** = server-side colocation + one round trip
  returning many objects (1 round trip regardless of N).

## Setup

Same topology as `ANYBLOB_BASELINE.md`: `neon-compute` as client,
`storage-backend` as server, WarpDrive on `:9710` restarted with
`STORAGE_BACKEND=slab` (true physical colocation, not just the metadata
hint) so the batch endpoint's read pattern matches its actual design
intent. Bucket `slab-batch-test`, objects uploaded with a shared
`x-warpd-slab` hint (via the `x-amz-meta-warpd-slab-hint` convention) so
they land in one contiguous on-disk region.

## First measurement — bad, and why

Initial naive-client test swept batch size `k = 1, 4, 16, 64, 256` (8 MiB
objects, so k=256 = 2GB) with a single signed HTTP GET, no concurrency.
Result: throughput **fell** as k grew (267 → 443 → 838 → 387 → 373 MB/s),
nowhere near AnyBlob's individual-GET numbers, let alone the network
ceiling. Three real, distinct bugs were found and fixed, in order:

### Bug 1: per-object file `open()` in the storage layer

`LocalXFSSlabStore::read()` (`server/src/storage/slab_store.rs`) opened a
fresh file handle on *every* read call. For k=256, that's 256 separate
`open()` syscalls against the same underlying slab file, done serially.

**Fix**: cache open file handles keyed by bucket path
(`READ_FILE_CACHE: RwLock<HashMap<PathBuf, Arc<File>>>`). Deliberately an
`RwLock`, not a `Mutex` — a cache entry is written exactly once and never
replaced (pread/`read_at` is safe to call concurrently on a shared handle
since it doesn't move a cursor), so concurrent reads only ever need the
shared read lock and never contend with each other; the exclusive write
lock is only taken the first time a given bucket is opened.

### Bug 2: unbounded `Vec` growth when buffering the response

To isolate whether per-chunk HTTP framing overhead (three tiny stream
chunks per object in the original streaming design) was the bottleneck,
`warpd_slab_batch_get` (`server/src/warpd.rs`) was changed to build the
whole multipart body in one buffer via repeated `body.extend_from_slice`
calls — which made things *worse* (down to 139 MB/s at k=256). Root
cause: growing a `Vec<u8>` toward ~2GB via `extend_from_slice` with no
pre-reserved capacity triggers reallocation at each capacity doubling,
and *each* reallocation copies the entire buffer built so far — multiple
full-buffer memcpys for a multi-GB body.

**Fix**: sum the known extent sizes up front and `Vec::with_capacity(...)`
before the read loop. This alone roughly halved `read_and_build_time` for
k=256 (6.85s → 3.59s), and further instrumentation (splitting `read_time`
from `copy_time`) confirmed disk reads were never the problem (page-cache
speed throughout) — the win was eliminating the memcpy storm.

### Bug 3: Nagle's algorithm never disabled on the server

Even after fixes 1-2, a **single plain object GET** (no batching at all,
one connection, one object, sizes 8MB-512MB) only reached 350-500 MB/s —
nowhere near the measured single-connection raw-TCP ceiling of 1362 MB/s
(see `ANYBLOB_BASELINE.md`'s network-ceiling section). Ruled out in order:
fixed per-request overhead (larger objects didn't scale throughput up,
so it isn't amortized-per-request cost), server auth/SQLite handler time
(WarpDrive's own `/_admin/metrics` showed 0.33ms average — negligible),
and per-request TCP handshakes (AnyBlob's connection pool defaults to
`reuse = 1`). What was left, and what actually explained it: **actix-http's
`tcp_nodelay` defaults to `None`** (`actix-http-3.12.1/src/builder.rs`) —
Nagle's algorithm was enabled on every connection WarpDrive ever accepted.
For a response body written in bounded internal buffer flushes, Nagle
batching interacting with the client's delayed-ACK timer stalls each
flush boundary — a well-known, classic TCP performance pathology that
exactly fits every symptom observed (capped well below single-connection
capacity, invisible to a raw synthetic socket test with no HTTP framing,
invisible to server-side handler timers that don't cover the socket-write
phase).

**Fix**: `.tcp_nodelay(true)` on the `HttpServer` builder in
`server/src/main.rs` — a one-line config change, zero architecture impact.

## Final results (median of 5 runs, warm page cache)

All numbers below are on the *same single TCP connection* (no client-side
concurrency anywhere in this table) so they isolate the primitive's value
from AnyBlob's parallelism story:

| Scenario | Median throughput | Range (5 runs) |
|---|---:|---:|
| Single 2GB object, 1 GET request | 514 MB/s | 470-615 |
| Sequential 256×8MiB individual GETs, 1 connection | 386 MB/s | 364-416 |
| **Batch-GET: 256 objects (2GB) in 1 request** | **359 MB/s** | 345-390 |
| Network ceiling, 1 raw TCP connection | 1362 MB/s | — |
| Network ceiling, 8 raw TCP connections | ~1985 MB/s | — |

(Run-to-run variance on the large single-shot transfers was substantial
before a warm-up pass was added — up to 3x on a single sample, most
likely `pd-balanced` disk quota / GCP network jitter rather than anything
in WarpDrive's own code, since the effect disappeared once page cache was
warm. All numbers above are post-warm-up medians, not single samples —
this matters when the swings are this large.)

## What this does and doesn't prove

**Does**: after three real fixes, WarpDrive's batch-GET primitive —
hit with a client that does nothing clever at all, one HTTP request, no
concurrency, no io_uring — achieves throughput within **~7% of** a
sequential loop of individual GETs at the same connection count (359 vs
386 MB/s), while using **1 round trip instead of 256**. That 7% gap is
almost certainly the remaining per-object bookkeeping in the batch loop
(256× `StorageService::new()`, 256× `format!()` header allocation, 256×
SQLite row materialization) — plausibly closeable, but diminishing
returns for further chasing today.

**Doesn't**: neither approach gets anywhere close to the ~1.9 GB/s network
ceiling on a single connection — that requires parallelism (multiple
connections), which is an orthogonal axis to batching, not something
either AnyBlob's concurrency or WarpDrive's colocation solves alone. The
honest version of the composability claim is: *batching collapses N round
trips into 1 at effectively the same per-connection throughput cost, with
zero client engineering* — not *batching beats parallelism*. Combining
batching with a small number of parallel connections (rather than
AnyBlob's dozens-to-hundreds) is the natural next step if higher absolute
throughput is the goal, and is cheap to test given everything already
built here.

## Combining batching with modest parallelism

Tested directly: split the same ~2GB corpus across multiple slab hints
(8 hints × 32 objects, then 16 hints × 16 objects, both 8 MiB each) and
fetch each hint's batch with one thread via a plain
`concurrent.futures.ThreadPoolExecutor` — no io_uring, no custom
networking code, no vendored AWS SDK, ~40 lines of ordinary Python calling
`requests` and `boto3`'s SigV4 signer.

| Approach | Median throughput (5 runs) | % of 8-conn network ceiling |
|---|---:|---:|
| Batch-GET, 1 connection (from above) | 359 MB/s | 18% |
| Batch-GET × 8 parallel threads (8 hints) | **1018 MB/s** | 51% |
| Batch-GET × 16 parallel threads (16 hints) | 956 MB/s | 48% |
| AnyBlob individual GETs, 8 connections (`t=4,c=2`, from `ANYBLOB_BASELINE.md`) | 1401 MB/s | 71% |

Two honest conclusions, both worth stating precisely:

1. **A trivial, ~40-line Python client gets WarpDrive's batching primitive
   to roughly half the network ceiling** — a ~3x improvement over the
   single-connection case — with no engineering beyond "run a few requests
   in a thread pool." 16 threads bought nothing over 8 (956 vs 1018 MB/s,
   within noise, both plateauing), most likely a Python/GIL-level ceiling
   in this specific trivial client rather than anything server-side —
   there was no attempt to push past this with a better client, since
   that would start reintroducing the engineering complexity this whole
   comparison is about avoiding.

2. **AnyBlob's io_uring engineering still wins on raw per-connection
   throughput** even at matched connection count (1401 vs 1018 MB/s at 8
   connections) — real, measurable, not something batching alone erases.
   But AnyBlob needs to issue and process one request per *object* to get
   that number (256 individual request/response cycles — SigV4 signing,
   HTTP parsing, server-side auth+SQLite lookup, all ×256), while the
   batched approach needs only **8** — one per hint, regardless of how
   many objects are in it. That gap in request count (not just connection
   count) is where batching's real, distinct value lives: less client and
   server CPU spent on repeated per-request bookkeeping, and — most
   concretely for the motivating use case — far fewer round trips for a
   one-time, cold-start-shaped fetch (e.g. Neon's pageserver prefetching
   the K delta layers for one checkpoint epoch at startup), which is a
   latency question, not a sustained-throughput one, and is exactly the
   shape AnyBlob's own benchmark methodology (many repeated requests
   against a warm connection) doesn't directly measure.

## Known remaining limitation

The buffer-the-whole-batch approach (fix 2) is hardcoded, not adaptive —
it trades back to the exact OOM risk the original streaming design was
written to avoid for very large batches. Not a problem for the batch
sizes tested here (≤2GB on a 32GB-RAM VM), but the honest fix is to make
this adaptive: stream in moderate-sized chunks (e.g. 16-32MB, not the
original design's 3-tiny-chunks-per-object, and not one unbounded buffer)
once a batch is too large to comfortably fit in memory, falling back to
today's eager buffering below that threshold. Not implemented today.

## Artifacts

All experiment scripts (bucket setup + client-side timing scripts for
every measurement in this document) are saved at
`logs/batch_get_investigation/scripts/`. Server-side timing instrumentation
output (every `SlabBatchGet timing` log line produced during this
investigation) is saved at
`logs/batch_get_investigation/slab_batch_get_timing.log`.

Code changes are in three files, all in `warpdrive/server/src/`:
`storage/slab_store.rs` (file-handle cache), `warpd.rs` (buffered batch
body + capacity pre-reservation + timing instrumentation), `main.rs`
(`tcp_nodelay(true)`).
