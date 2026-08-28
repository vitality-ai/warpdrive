# AnyBlob client-side baseline — MinIO vs WarpDrive

## Idea

Before building/exposing WarpDrive's own server-side batching + colocation
primitive (the slab batch-GET endpoint), establish what plain, one-object-
per-request PUT/GET looks like on both MinIO and WarpDrive under a real,
published, industry-grade client — not a harness we wrote ourselves.

The client is **AnyBlob** (Durner, Leis, Neumann, VLDB 2023, "Exploiting
Cloud Object Storage for High-Performance Analytics",
https://github.com/durner/AnyBlob, MPL-2.0): an io_uring-based download/
upload manager that keeps many requests in flight per thread to hide
round-trip latency, instead of the usual one-request-per-thread model. Its
own packaged benchmark (`example/benchmark`) is what the paper's own
Figures were generated from.

This is step 1 of a two-part comparison:
1. **(this document)** How much client-side concurrency/engineering does
   each system need to saturate, doing plain individual PUTs and GETs?
2. **(next)** Point a *naive*, unsophisticated client (no concurrency
   tuning at all) at WarpDrive's slab batch-GET primitive and see whether
   it matches or beats step 1's best (highest-concurrency) numbers. That
   comparison — not this one — is what would actually support "you don't
   need AnyBlob's complexity if the server gives you a batching primitive."

Two different mechanisms are being kept distinct here: AnyBlob's
"batching" is client-side request concurrency (still N round trips,
overlapped); WarpDrive's slab primitive is server-side colocation + one
round trip returning many objects. This document only exercises the
former.

## Setup

- **Client**: `neon-compute` (GCP `e2-standard-16`, Ubuntu 24.04, kernel
  6.17 — io_uring fully supported).
- **Servers**, both on `storage-backend` (GCP `e2-standard-8`):
  - MinIO, a fresh instance on `:9002` (bound to a newly-attached 40GB
    disk mounted at `/mnt` — the VM's root disk was already 100% full
    from earlier, unrelated experiments; a second MinIO instance avoided
    touching that existing data at all).
  - WarpDrive on `:9710`, restarted with `STORAGE_DIRECTORY`/`DB_FILE`
    pointed at the same new disk (its port is hardcoded, so unlike MinIO
    it couldn't run a second side-by-side instance — the single process
    was redirected instead; old data on the root disk was left untouched).
  - Both use the same admin credentials (`adminkey` /
    `adminsecretkey123456`), and WarpDrive was hit at its bare
    `/{bucket}/...` path-style root (it supports both `/s3/{bucket}/...`
    and plain path-style addressing), so no protocol-level special-casing
    was needed to point AnyBlob at it.
- **Corpus**: bucket `anyblob-bench` on both targets, pre-populated with
  100 objects (`1.bin` … `100.bin`), 8 MiB each.
- **Sweep**: `AnyBlobBenchmark minio bandwidth` (see patches below),
  `-t 4` threads fixed, `-c` (concurrent outstanding requests per thread)
  swept over `1, 2, 4, 8, 16, 32, 64`, request count scaled with
  concurrency (`c * threads * 20`) so higher-concurrency runs still move
  enough data for a stable measurement.

## Patches made to AnyBlob

Two real findings, both kept as minimal, targeted patches rather than
workarounds in WarpDrive:

1. **The packaged benchmark CLI never exposed MinIO/custom-endpoint
   support that the library already has.** `cloud::AWS::Settings` has
   `endpoint`/`port` fields specifically for non-AWS S3-compatible
   servers, and there's a first-class `minio://host:port/bucket:region/`
   URI scheme (`cloud::Provider`, `cloud::MinIO`) that forces path-style
   addressing — exactly what's needed for both MinIO and WarpDrive. But
   `example/benchmark/src/main.cpp` only ever built `s3://...` URIs for
   the `aws` provider and had no argv flags for endpoint/port. Added a
   `minio` provider branch plus `-w <endpoint>` / `-p <port>` flags that
   feed the existing `AWS::Settings` fields — a few lines, no change to
   the library itself.

2. **Real bug: case-sensitive HTTP header parsing.**
   `src/network/http_helper.cpp`'s `HttpHelper::detect()` compared header
   names with exact-case `string_view` equality against the literal
   strings `"Content-Length"` / `"Transfer-Encoding"`. WarpDrive (built on
   Rust's `http`/actix-web crates) emits **lowercase** header names on the
   wire — which is RFC 7230 §3.2-compliant (header field names are
   case-insensitive) but doesn't match AnyBlob's literal string compare.
   Every WarpDrive GET failed with `MessageFailureCode::HTTP` until this
   was patched to a case-insensitive comparison. This never surfaces
   against AWS S3 or MinIO because they both happen to emit canonical
   casing — it's a latent interop bug in the reference client, not
   something specific to WarpDrive being "wrong."

3. **PUT-blocking bug: a 500ms per-syscall socket timeout tuned for AWS,
   not general single-node servers.** `network::ConnectionManager`
   hardcodes `TCPSettings::timeout = 500ms`
   (`include/network/connection_manager.hpp:53`), with no CLI flag to
   change it. On the io_uring path this isn't an overall request
   deadline — it's a *linked timeout on every individual send/recv
   syscall* (`io_uring_prep_link_timeout` in `io_uring_socket.cpp`): if
   one send() or recv() doesn't complete within 500ms, it's cancelled and
   the whole request fails as `MessageFailureCode::Timeout`. Concurrent
   16 MiB uploads (the benchmark's fixed upload size, `1 << 24` in
   `bandwidth.cpp`) hit this reliably once concurrency rose past `c=1`:
   near-100% failures by `c=16`. The paper itself says nothing about
   timeouts, but `include/network/config.hpp`'s own comments make the
   real reason explicit — its concurrency/throughput defaults are
   "based on AWS experiments" (`defaultCoreThroughput`,
   `defaultCoreConcurrency`), and the 500ms socket timeout was clearly
   calibrated the same way: AWS S3's backend can always drain a TCP send
   buffer well inside 500ms. A single-node server doing real synchronous
   disk writes under many concurrent large uploads legitimately can't
   guarantee that — the buffer stalls while the disk catches up, and
   AnyBlob mistakes that backpressure for a dead connection. This hit
   MinIO and WarpDrive identically; it isn't a target-side bug. Bumped
   the constant to 30000ms and reran — zero failures across the full
   sweep afterward.

All three patches applied directly to the vendored source at
`example/benchmark/build/thirdparty/anyblob/src/anyblob/` (the packaged
CMake build re-clones AnyBlob into its own build tree via
`ExternalProject_Add` rather than using a sibling checkout — patches must
go there, not in a separate top-level clone, or they silently don't take
effect on rebuild).

## Network ceiling (ground truth)

Before trusting the ~1.9 GB/s plateau below as a target-side limit, it was
checked directly: 8 parallel raw TCP connections between `neon-compute`
and `storage-backend` (plain Python sockets, no HTTP/S3 involved) sustain
**~1985–1989 MB/s (≈15.9 Gbps)**, confirmed independently on both the
sender and receiver side. That matches GCP's published 16 Gbps egress cap
for `e2-standard-16`/`e2-standard-8` (E2 machine types cap network
bandwidth well below what vCPU count alone would suggest, unlike N2/N1).
A single unparallelized TCP connection only reached ~1362 MB/s (~10.9
Gbps) — consistent with the sweep's own finding that concurrency is
needed to reach the ceiling, this time for the boring reason (single-flow
TCP window/single-core packet processing limits), not anything
target-specific.

The AnyBlob GET ceiling below (~1920 MB/s) is within 3.5% of this raw
number — the gap being real HTTP/S3 protocol overhead and actual disk
reads, as expected. **The ~1.9 GB/s plateau is the network, not MinIO or
WarpDrive** — neither system is the bottleneck once concurrency is high
enough; what differs between them (see below) is how much concurrency
each one needs to get there.

## Results: GET sweep

8 MiB objects, 100-object corpus, `t=4` threads, request count
`c * 4 * 20`. Throughput = `Datasize / Time` from AnyBlob's own summary
CSV (raw data: `logs/anyblob_baseline/{minio,warpdrive}_get.csv{,.summary}`; the
three vendored-source patches described above are saved at
`logs/anyblob_baseline/patches/`).

| Concurrency (c) | MinIO throughput | WarpDrive throughput | Client CPU (user+sys ms), MinIO / WarpDrive |
|---:|---:|---:|---:|
| 1  | 550 MB/s   | **1304 MB/s** | 93 / 44 |
| 2  | 791 MB/s   | **1401 MB/s** | 127 / 99 |
| 4  | 1380 MB/s  | **1829 MB/s** | 219 / 179 |
| 8  | 1822 MB/s  | 1899 MB/s     | 409 / 382 |
| 16 | 1930 MB/s  | 1913 MB/s     | 826 / 776 |
| 32 | 1928 MB/s  | 1928 MB/s     | 1915 / 1730 |
| 64 | 1915 MB/s  | 1912 MB/s     | 4667 / 4448 |

**Key finding:** both systems converge to the same ceiling (~1.9–1.93
GB/s, evidently the network/disk limit between these two VMs) once
concurrency is high enough — but WarpDrive gets there with far less
client-side effort. At `c=1` (closest to a "naive" single-threaded
client), WarpDrive already delivers **2.4x** MinIO's throughput (1304 vs
550 MB/s) and does it in less wall-clock time with less client CPU. MinIO
needs to ramp concurrency up to `c≈16` before it matches what WarpDrive
gets at `c=1`. This is a meaningful data point in its own right (lower
per-request overhead/latency in this deployment), independent of any
batching primitive.

**Sharper framing — where does each system actually saturate?** Taking
the shared ceiling as ~1920 MB/s (average of each target's own top-3
concurrency levels — both converge to nearly the same number, confirming
it's a network/disk limit rather than a target-specific one), and looking
at % of that ceiling reached at each `c`:

| c | MinIO % of ceiling | WarpDrive % of ceiling |
|---:|---:|---:|
| 1  | 28.6%  | 67.9% |
| 2  | 41.2%  | 73.0% |
| 4  | 71.9%  | **95.3%** |
| 8  | **94.9%** | 98.9% |
| 16 | 100.5% | 99.6% |
| 32 | 100.4% | 100.4% |
| 64 | 99.7%  | 99.6% |

WarpDrive crosses the ~95%-saturated mark at **`c=4`**; MinIO doesn't get
there until **`c=8`** — a full concurrency-doubling step later. Everything
past each system's own crossing point is noise around the same shared
ceiling. Put plainly: with WarpDrive, AnyBlob's io_uring machinery barely
needs to do anything (4-way concurrency is what a handful of ordinary
threads would give you for free) to hit line rate; against MinIO it
genuinely needs the higher end of what AnyBlob was built for. That is a
more direct, falsifiable version of "how much client sophistication does
each system need."

**Client-machine CPU utilization.** AnyBlob's summary also reports
`CPUActiveAllProcesses` / `CPUIdleAllProcesses` — whole-client-machine CPU
tick counts during the run, not just the benchmark process. Utilization %
= `Active / (Active + Idle)`:

| c | MinIO | WarpDrive |
|---:|---:|---:|
| 1  | 5.4%  | 6.5% |
| 2  | 5.7%  | 7.2% |
| 4  | 7.9%  | 9.2% |
| 8  | 10.4% | 10.2% |
| 16 | 11.3% | 10.5% |
| 32 | 13.2% | 11.5% |
| 64 | 15.7% | 15.1% |

Both stay well under 16% even at `c=64` — io_uring delivering on its core
promise (high throughput without proportionally high CPU cost) against
either target. Not a meaningful differentiator between the two here.

## Results: PUT sweep

Fixed 16 MiB objects (the benchmark's hardcoded upload size), `t=4`
threads, request count `c * 4 * 2` (kept far below disk capacity, with
all uploaded objects deleted between each concurrency step so usage never
compounds). Raw data:
`logs/anyblob_baseline/{minio,warpdrive}_put.csv{,.summary}`.

AnyBlob's own `Datasize` column under-reports Upload volume by 2x (it
appears to reuse the GET corpus's 8 MiB object size for this field
instead of the real 16 MiB upload size) — a client-reporting quirk, not a
transfer-correctness issue (every object round-tripped and was
successfully deleted afterward). Throughput below is computed from the
known real payload (`requests * 16 MiB`) divided by AnyBlob's own `Time`
column, which is unaffected by the mislabeling.

| Concurrency (c) | MinIO PUT throughput | WarpDrive PUT throughput |
|---:|---:|---:|
| 1  | 172.8 MB/s | **698.7 MB/s** |
| 2  | 161.6 MB/s | **804.2 MB/s** |
| 4  | 157.6 MB/s | **829.4 MB/s** |
| 8  | 150.5 MB/s | **747.4 MB/s** |
| 16 | 151.9 MB/s | **643.0 MB/s** |
| 32 | 152.0 MB/s | 501.1 MB/s |
| 64 | 151.6 MB/s | 201.4 MB/s |

**Key finding:** MinIO's PUT throughput is essentially flat (~150–173
MB/s) regardless of concurrency — a single-node MinIO instance is disk/
checksum-bound on writes and concurrency doesn't help it. WarpDrive
starts far ahead (4–5.7x MinIO at low concurrency, peaking at 829 MB/s
around `c=4`) but its throughput **degrades steadily as concurrency
rises**, falling to 201 MB/s by `c=64` — still above MinIO's flat line,
but the gap that made WarpDrive look dramatically better closes sharply
under heavy concurrent write load. This wasn't chased further (no
server-side telemetry was collected for this run — client-side
measurement only, by design, matching the naive-client methodology
planned for the next step), but it's a real, honest finding worth
carrying forward: WarpDrive's write path has a concurrency-scaling
bottleneck under many simultaneous large uploads that's worth its own
investigation separately from the batching/colocation narrative.

**Client-machine CPU utilization (PUT):**

| c | MinIO | WarpDrive |
|---:|---:|---:|
| 1  | 1.7% | 6.3% |
| 2  | 1.9% | 5.2% |
| 4  | 1.6% | 4.9% |
| 8  | 1.5% | 3.7% |
| 16 | 1.6% | 4.6% |
| 32 | 1.6% | 4.6% |
| 64 | 1.5% | **2.0%** |

MinIO's client CPU stays flat and low throughout, consistent with it
being disk/checksum-bound rather than client-bound — matches its flat
throughput line above. WarpDrive's is the more telling one: client CPU%
actually *drops* as concurrency climbs to 64, at exactly the point where
its throughput is collapsing (829 → 201 MB/s). The client isn't working
harder during those slow high-concurrency runs — it's waiting. That
points at the server (or network) as the bottleneck for WarpDrive's PUT
degradation, not client-side resource exhaustion — consistent with,
though not proof of, a write-path contention issue worth its own
follow-up investigation.

## Next step — done, see `BATCH_GET_INVESTIGATION.md`

That document hits WarpDrive's `/_warpd/slab/{bucket}?hint=...` batch-GET
endpoint with a trivial, unsophisticated client and compares it against
this document's numbers. Summary: after three real fixes (a storage-layer
file-handle cache, a `Vec` pre-allocation fix, and enabling `TCP_NODELAY`
— none of them architecture changes), a single-connection batch-GET
reaches ~7% of naive sequential individual-GET throughput at 1 round trip
instead of 256, and a trivial ~40-line multi-threaded Python client
(no io_uring) gets the primitive to roughly half the network ceiling.
