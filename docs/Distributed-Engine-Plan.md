# WarpDrive: Real Distributed, Erasure-Coded Object Engine with Pluggable Placement

## Context

This plan is about building a real distributed WarpDrive. It is not a HiPC-poster task —
the poster is a separate, parked piece of work (currently 3 pages, retrim decision
pending, tracked independently). The only thing this plan takes from that context is a
deadline: a real distributed engine, not a Python simulator, needs to exist before
Oct 8, 2026 AOE (today is Oct 2 — **6 days**), because the poster's current numbers come
from a simulator (`hipc_poster/`) and the user wants real measurements to exist instead.
Everything else below is scoped purely to WarpDrive itself.

A read-only codebase survey (`/Users/cj/cj/personal/claude/warpdrive-distributed`, cloned
fresh from `vitality-ai/warpdrive`, `main` @ `dc53849`) found:

- **Zero distributed code exists today.** Single process, single SQLite metadata DB, one
  flat file per bucket (`server/src/storage/local_store.rs`), one `actix-web` server on
  `:9710`. No RPC, no membership, no consensus, no erasure coding library, no placement
  logic anywhere.
- `tla/README.md` and `docs/Technical-Roadmap.md` both explicitly defer distribution and EC
  to future work ("seeking design contribution"). RFC3/RFC3.1 (segment storage, block
  device) discuss EC only as an explicit **Non-Goal**, and neither is implemented in code —
  `local_store.rs` still matches RFC1's original flat-file design.
- **Reusable seams**: a clean `Storage` trait (`write`/`read`/`delete`/`verify`) with a
  single caller (`StorageService`), and a metadata layer (SQLite) already separated from
  the data layer, with an existing pattern for per-bucket settings (versioning, ACL,
  object-lock). The mature S3 API surface (RFC2 series, mostly merged) needs no changes.
- No GCP/multi-node deployment scripts, no distributed benchmark harness. The
  `docs/benchmarks/scaling_slab` data referenced earlier this session **does not exist in
  this checkout** — do not plan around it.

User's explicit direction for the architecture (overriding my first, more conservative
proposal of a Python client-side orchestrator over unmodified nodes):

> "We'll first build a truly distributed object engine... by default we'll function as a
> normal fully distributed object storage [with] custom placement policies... We can have
> multiple co-ordinators just like what fusion does. We'll borrow ideas from industry
> systems but use techniques to solve the problem at hand instead of following what others
> have done. Each line of code should come from explainability, and if there are
> bottlenecks we document and move on."

**Revised direction from the user after the first pass of this plan:** get the core
distributed engine built fast, correct, and genuinely fast at runtime, before touching
content-dependent placement at all. Content-dependent placement (the poster's own
contribution) is explicitly **deferred**: the system should be designed so it is
extensible and composable for later, but the only placement policy actually implemented
in this phase is the default, computed one. The user also asked for a reference survey of
recent industry/academic techniques first, "just for reference," not a prescription to
adopt wholesale, plus a check of WarpDrive's own `docs/Technical-Roadmap.md` for systems
already under study there.

This plan designs a real, minimal-but-genuine distributed object engine, built inside
WarpDrive's own Rust codebase: one concrete, explainable placement policy (computed,
CRUSH-style) implemented now, with a `PlacementPolicy` trait boundary left open for
content-dependent placement later, symmetric multi-node coordination (no dedicated
coordinator, any node can serve a request — matching Fusion's own peer architecture), and
explicitly documented non-goals so scope stays achievable in 6 days. Priority order:
**correct and fast first, extensible second, content-dependent placement later.**

## Background survey (reference only, not a prescription)

WarpDrive's own `docs/Technical-Roadmap.md` already tracks relevant background, including
two directly useful pointers its own authors flagged:
- **Chain replication**~\cite{chainrepl2004} (van Renesse & Schneider, OSDI'04) — the
  roadmap itself says this "would matter if/when the erasure-coding fault-tolerance work
  gets designed," i.e. the project's own prior art search already earmarked this as
  relevant to exactly the work this plan is doing.
- **ShardStore**~\cite{shardstore2021} (Amazon S3's formally-verified backend, SOSP'21) —
  cited there as "a reminder of how seriously a 'boring' object storage layer has to be
  taken once real systems depend on it, a reminder, not a design to reuse." Relevant to
  how much correctness-testing rigor this new cluster layer deserves.
- Cachestack (Google, ATC'22, knapsack SSD/HDD placement) and per-file virtualization for
  PM filesystems (FAST'23) are flagged there as placement ideas "at a different
  granularity" — relevant context for `PlacementPolicy`'s design, not something to port.

A separate literature-survey fork is researching QuePaxa (leaderless/multi-proposer
consensus, relevant to the symmetric multi-coordinator requirement), recent erasure-coded
placement papers adjacent to Fusion/CRUSH/MapX, low-latency storage-node architecture
techniques (io_uring, SPDK, shard-per-core), and metadata/location-map design at scale.
*[To be folded in once that fork reports back — treat as reference material for design
rationale, not a to-do list of things to implement in 6 days.]*

## Architecture

**One binary, many roles.** Every deployed WarpDrive process is both a *storage node*
(stores shards via the existing, unmodified `Storage` trait and local disk) and a
*coordinator* (can receive a client request, decide placement, and fan out to peers over
their existing HTTP API). This is the "multiple coordinators" the user asked for: no
special coordinator process, no single point of failure for request routing — symmetric,
like Fusion's own design.

**Every component is a trait with one concrete implementation today, not a concrete
type.** This is not a "design for the future" abstraction (explicitly against this
project's own rule of not building for hypotheticals) — it is the specific, named
requirement from phase 3 (`ContentDependentPlacement` must drop in later without
touching `coordinator.rs`) applied consistently across the whole module, so the same
seam exists everywhere it will plausibly be needed, not just in `placement.rs`:

- `PlacementPolicy` (`placement.rs`) — `ComputedPlacement` now, `ContentDependentPlacement`
  later, same trait, same call site.
- `Storage` (already exists, unmodified) — local-disk impl now; the io_uring decision
  point (phase 1 step 7) swaps its internals behind this same trait if profiling warrants
  it, with zero callers changed.
- A small `LocationStore` trait wraps `location_store.rs`'s Bitcask-style implementation,
  so the SQLite-vs-custom-store comparison in Verification is a swap of one implementation
  for another behind the same interface, not a rewrite of `coordinator.rs`.
- `ErasureCoder` (`ec.rs`) wraps `reed-solomon-erasure` behind a narrow `encode`/`decode`
  interface, so a different EC library or parameters can be swapped later without
  touching `coordinator.rs`.
- `PeerClient` (`peer_client.rs`) — `put_shard(peer, bucket, key, idx, data)` /
  `get_shard(peer, bucket, key, idx) -> data`, the inter-node transport. See below: this
  is the one place a second implementation is built *and benchmarked* in phase 1 itself,
  not deferred, per explicit user direction to use gRPC/FlatBuffers instead of HTTP "if
  that's faster."

Each trait boundary exists because a specific, already-identified future need crosses it
(phase 3 placement, the io_uring decision point, the SQLite-vs-custom comparison,
HTTP-vs-gRPC transport) — not speculative generality. `coordinator.rs` only ever calls
these five interfaces, never a concrete type directly, which is what makes each one
swappable in isolation.

**Why no leader election, checked against a real system.** MinIO's own docs confirm this
is how a production EC object store actually works, not just a shortcut for lack of time.
Placement is "deterministic, stateless": every server hashes the object name the same way
and dispatches to all shards in parallel, with no node needing to ask another where
anything lives. Consistency comes from quorum, not consensus: MinIO's read quorum is
exactly `k` (the data-shard count) and write quorum is `k` as well, bumped to `k+1` only in
the edge case where parity count equals data-shard count. That formula maps directly onto
our own `RS(k, m)` notation already used throughout the paper. We adopt the same rule:
reads succeed once `k` shards respond, writes succeed once `k` peers acknowledge (`k+1` if
`m == k`). Leader election matters for things this phase deliberately doesn't do yet
(strongly-ordered membership changes, automatic rebalancing of existing data) — it
doesn't matter for serving PUT/GET against a known node set, which is phase 1's entire
scope.

**Leader election and dynamic membership are two different questions, checked across
three real systems, not just one.** An earlier draft of this plan conflated them into a
single non-goal. They are not the same thing:

- **MinIO**: no leader election, but does support growing a cluster, via "server pools."
  A new pool registers (hot-reloadable in newer versions, a restart in older ones) and
  immediately takes new writes. Existing objects never move to the new pool.
- **Ceph**: no leader election for data placement either (CRUSH is also a deterministic
  hash), but it goes further: adding or removing an OSD updates the CRUSH map, and Ceph
  *automatically migrates existing data* to match the new map. This is real engineering
  (an OSD map epoch, background rebalancing traffic) and is the one piece genuinely out
  of reach in 6 days.
- **SeaweedFS**: no leader election. A new volume server registers with the master and
  can serve new writes immediately, no restart. Explicitly does **not** rebalance
  existing data unless an admin triggers it.

None of the three need consensus just to let a node join. What's actually expensive is
*rebalancing data that already exists* when membership changes, and only Ceph does that
automatically. Given this, phase 1 upgrades from a purely static peer list to a cheap,
SeaweedFS/MinIO-style **additive registration**: a new node can join and start serving new
writes without a restart, but placement decisions already made for existing objects are
never recomputed or moved. See `membership.rs` below.

**Write path computes placement fresh and pins it; read/delete paths never recompute —
this is what actually makes "never moved" true.** `ComputedPlacement`'s hash output
changes whenever the peer list's size changes. Recomputing it on every read would
silently reshuffle which peers a key maps to the instant any node joins, contradicting
the guarantee above — this is the actual mechanism, not just an assertion. So: **PUT**
resolves `ComputedPlacement` against the coordinator's current peer list, fans out, and
then records the resolved peer set into `location_store.rs` as that key's pinned
assignment. **GET and DELETE never call `ComputedPlacement`** — they look up the pinned
assignment in `location_store.rs` and use that, regardless of what the peer list looks
like now. `ComputedPlacement` itself uses rendezvous (highest-random-weight) hashing,
not a naive modulo, so even the write-time resolution is well-behaved: a membership
change only ever affects the specific keys that would newly hash to the joining node,
not a wholesale reshuffle of everything.

**New code lives entirely in a new `server/src/cluster/` module.** Nothing in
`local_store.rs` or the `Storage` trait needs to change. Cluster logic is a layer in
front that, on the write path, decides which shards go to which peers and sends each one
over `PeerClient`; on the read path, decides which peer(s) to fetch from and reads each
shard the same way.

**Inter-node transport: HTTP first, gRPC/FlatBuffers as a benchmarked alternative, not an
assumption.** The original plan reused the existing S3 PUT/GET endpoints verbatim
(zero new protocol, 9.7k lines of already-tested code). Per direct user direction, that's
now the *first* `PeerClient` implementation, not the only one: plain HTTP via `reqwest`,
calling the S3 endpoints with shards addressed as objects in a reserved bucket namespace
(`bucket = "__cluster__"`, `key = "{bucket}/{key}/shard{i}"`). This is what phase 1 step 3
builds and benchmarks first, because it has zero marginal setup cost and unblocks
correctness work immediately. Right after that number exists, a second `PeerClient`
implementation over gRPC (`tonic`+`prost` for the service, the shard payload itself
FlatBuffers-encoded, reusing the `flatbuffers` crate already in this codebase rather than
adding a second serialization format) is built and benchmarked head-to-head on the same
single-shard put/get latency, localhost, same shard size. Whichever wins becomes the
default; the loser stays in the tree behind the trait, documented, not deleted — this
mirrors exactly how the io_uring decision point already works below, and for the same
reason: a transport swap is cheap to evaluate with real numbers and expensive to assume.
**Honest cost note:** gRPC via `tonic` needs a `.proto` file, `tonic-build` codegen, and a
local `protoc` install — real setup cost HTTP doesn't have. That cost is paid once,
during this decision point, specifically because the user asked for the comparison, not
assumed to be worth it a priori.

**Components (`server/src/cluster/`), phase 1 scope only:**

- `membership.rs` — starts from an env var (`WARPDRIVE_PEERS=http://node1:9710,...`),
  following the existing `StorageConfig::from_env` pattern, plus one cheap addition:
  a `POST /cluster/join` handler, SeaweedFS/MinIO-pool-style. A new node calls it on any
  existing peer, is added to that peer's in-memory list, and that list is handed back in
  the response so the new node (and, via one gossip-free broadcast round, every other
  known peer) converges on the same set. No consensus, no ordering guarantee beyond
  "eventually every node's list agrees" — acceptable because placement for a given
  `(bucket, key)` is computed fresh from whatever peer list a node currently has, and
  **existing objects are never moved when the list changes** (matches SeaweedFS's
  explicit choice, not an oversight). **Documented non-goal**: automatic rebalancing of
  already-placed data on membership change (Ceph's model) — real engineering effort,
  genuinely out of scope for 6 days, and separable from membership itself.
- `placement.rs` — a `PlacementPolicy` trait, with **one** real implementation now:
  - `ComputedPlacement`: CRUSH-style~\cite{crush2006} deterministic hash of
    `(bucket, key)` → a subset of `k+m` peers. This is the **default** and, for this
    phase, **only** policy, giving "normal fully distributed object storage" out of the
    box, with no per-object state beyond the hash function itself (matches the poster's
    own "computed" category, `O(buckets)` metadata).
  - The trait is designed so a second implementation (content-dependent, Fusion-style
    bin-packing keyed on a client-supplied computable-unit list) can be dropped in later
    without changing `coordinator.rs` or the wire format — **not implemented now**. The
    `x-warpd-computable-units` header and the per-bucket granularity knob are explicitly
    deferred to a later phase.
- `ec.rs` — an `ErasureCoder` trait (`encode(data) -> k+m shards`,
  `decode(available_shards, original_len) -> reconstructed`), with one implementation
  (`ReedSolomonCoder`) wrapping the `reed-solomon-erasure` crate. `ReedSolomonCoder::new(k, m)`
  already takes `(k, m)` as constructor arguments, not a hardcoded global — the trait was
  never the obstacle here. Fixed-size shards for now (whole object padded to a configured
  shard size), not the stripe/bin-packing model — that coupling to `fac_core.py`'s
  overhead formula is part of the deferred content-dependent work, not this phase.
  **Per-bucket/per-user `(k, m)`, deferred but designed for.** Checked against three real
  systems: MinIO resolves EC parity per *object*, but only from a small, operator-defined
  set of named presets (`STANDARD`/`REDUCED_REDUNDANCY`, selected via an
  `x-amz-storage-class` header, each preset's actual `EC:M` value fixed server-side via
  env var) — never an arbitrary client-chosen `(k, m)`. Ceph resolves it per *pool* (a
  named erasure-code profile), with a bucket mapped to a pool. Azure resolves redundancy
  only at the *storage-account* level, coarser than per-bucket. The consistent pattern:
  real systems let `(k, m)` vary at a granularity coarser than "whatever the client
  wants," via named presets an operator defines, not a free parameter. Phase 1 ships one
  hardcoded preset (see default parameters below). The deferred work (like content-
  dependent placement) is a small per-bucket lookup, MinIO-style, that resolves a
  bucket's storage class to an `ErasureCoder` instance — `coordinator.rs` would call that
  lookup the same way it already calls `PlacementPolicy` per bucket, so this costs
  nothing to design for now and isn't a rewrite later.
- `coordinator.rs` — new HTTP handlers (`PUT/GET /cluster/{bucket}/{key}`) that: resolve
  the bucket's placement policy, split/encode the object into `k+m` shards, and call each
  owning peer's `PeerClient::put_shard`/`get_shard` **concurrently**
  (`tokio::join!`/`futures::future::join_all`, not sequentially), whichever `PeerClient`
  implementation wins the transport decision point below. This is the single
  highest-value, lowest-risk speed lever available in days, not weeks: coordinator
  latency becomes `max(peer RTTs)` instead of their sum. Write succeeds once `k` peers
  acknowledge (`k+1` if `m == k`); read succeeds once `k` shards respond, per the quorum
  rule above. PUT resolves placement fresh and writes the pin to `location_store.rs`;
  GET and DELETE read the pin instead of recomputing (see above).
- `location_store.rs` — **not SQLite.** A `LocationStore` trait (`put(key, shard_list)`,
  `get(key) -> shard_list`) with one implementation, Bitcask-style: an append-only log
  file per node (one record per PUT: key → shard list) plus an in-memory hash index
  (`HashMap`/`DashMap`) rebuilt by replaying the log on startup. Per direct operational
  feedback that SQLite becomes a real bottleneck under this kind of write pattern, the
  cluster's per-object location map (which peer holds which shard) gets this
  purpose-built store instead of reusing WarpDrive's existing single-node SQLite metadata
  layer. This is the same pattern already present as a reference in this repo's own
  RFC3.1 literature survey (Bitcask is listed there), just applied to cluster metadata
  instead of general bucket/object metadata. Replicated across the same `k+1` peers
  Fusion itself replicates its chunk-location map across. The trait boundary is what
  makes the Verification section's SQLite-vs-custom-store comparison a swap of
  implementations, not a rewrite. **WarpDrive's existing single-node SQLite metadata
  layer (`metadata/sqlite_store.rs`) is untouched** — it keeps serving single-node
  deployments exactly as today; this is a new, separate store used only by the cluster
  layer.
- **Object lock (WORM retention), piggybacked on the same `location_store.rs` record, no
  distributed lock manager.** SeaweedFS is a useful real data point here, and its own
  GitHub issues are informative about what to avoid, not just what to copy: object lock
  there is metadata-driven (retention mode, retain-until date, legal hold stored as
  extended attributes on the entry), enforced by checking that metadata at delete time —
  there is no lock to acquire, because WORM retention is a policy check, not a mutual-
  exclusion problem. But SeaweedFS has had real bugs here (seaweedfs#8350, #11333:
  `COMPLIANCE` retention accepted by the S3 gateway, but `DeleteObject` on the locked
  version still succeeded) because enforcement was split across two separate code paths
  — the gateway that accepts the retention config, and the filer's delete path — that
  could drift out of sync. We avoid that specific failure mode structurally rather than
  by being more careful: add `retention_mode`, `retain_until`, and `legal_hold` fields
  directly onto the *same* per-key record in `location_store.rs` that already pins shard
  placement. There is exactly one record and one lookup for "where are this object's
  shards" and "is this object currently locked" — not two systems that can disagree.
  Whichever peer handles a DELETE or an overwriting PUT looks up that one record and
  enforces the retention fields locally. Because the record is already replicated to the
  same `k+1` peers as the placement pin, enforcement is consistent across replicas for
  free — the same "deterministic, stateless, quorum not consensus" principle used for
  placement, applied to retention instead of inventing a second mechanism.

**Storage backing: loopback device + XFS, per node.** Per user request, each node's data
directory is backed by a real block device instead of whatever filesystem the host
happens to provide: `fallocate` a backing file, `losetup` it to a loop device, `mkfs.xfs`
it, mount it at the path `local_store.rs` already uses as its storage directory. No code
change to `local_store.rs` itself is required for this part — it is a deployment-time
step (a short setup script), since the `Storage` trait already treats its directory as
opaque. This is cheap, directly testable (one VM, one loop device, one `mkfs.xfs` call),
and gives real extent-based allocation instead of relying on host-filesystem behavior —
matching what RFC1 and RFC3.1 already discuss wanting. **Caveat, confirmed not just
theoretical:** the local dev machine this session runs on is macOS (Darwin), where
`losetup`/`mkfs.xfs` don't exist at all (`which` confirms neither binary is present) —
this is a Linux-only technique, not a permissions issue solvable with `sudo`. The setup
script is written now (`scripts/setup_xfs_storage.sh`) but only runs, and only gets its
benchmark number, on the real Linux GCP VMs in phase 2. Phase 1's local dev loop (steps
3-8) uses the plain local filesystem via the existing, unmodified `Storage` trait —
nothing in those steps depends on XFS being present, so this isn't a blocker for them.

This also opens a concrete, low-risk stretch goal, not required for phase 1 but worth
doing if time allows once XFS is in place: the codebase survey found that deletes in
`local_store.rs` are queued but never actually reclaim disk space today. With a real
block device under XFS, a delete can call `fallocate(FALLOC_FL_PUNCH_HOLE |
FALLOC_FL_KEEP_SIZE, offset, len)` (via the `nix` crate) to actually punch the hole and
free the space, fixing that documented gap as a direct side effect of adding XFS rather
than as separate scope. Document as done or deferred, per the project's own "document
bottlenecks and move on" rule, if it doesn't fit.

**Explicit non-goals for this phase (documented, not silently dropped):**

- Content-dependent placement, the `x-warpd-computable-units` header, and the per-bucket
  granularity knob. Deferred by explicit user direction. The `PlacementPolicy` trait
  boundary is the only thing built now in anticipation of it.
- Per-bucket/per-user `(k, m)` selection (a storage-class-style preset lookup, see
  `ec.rs` above). Phase 1 ships one hardcoded preset; `ErasureCoder` already takes
  `(k, m)` as constructor arguments, so this is a lookup to add later, not a rewrite.
- Consensus (Raft, PBFT) for membership changes, and automatic rebalancing of
  already-placed data when the peer list changes (Ceph's model). Joining itself is cheap
  and in scope (see `membership.rs` above); moving existing data to match a new
  membership is not.
- Decode-from-failure as a steady-state read path. EC decode is implemented and
  correctness-tested once (kill a shard, confirm reconstruction); steady-state reads fetch
  data shards directly.
- Authentication beyond WarpDrive's existing admin-key bypass.
- Cluster-wide `ListBuckets`/`ListObjectsV2`. Each node's existing single-node
  `sqlite_store.rs` is untouched and local, so no node has a complete cluster-wide view
  of what exists. Phase 1 exposes only `PUT`/`GET`/`DELETE` on a known key; listing
  across the cluster would need its own design (e.g. scanning `location_store.rs` on
  every node) and is deferred.
- Versioning and ACL on the new cluster endpoints (the existing single-node S3 API keeps
  both, unchanged). **Object lock is the one exception** — it is in scope, see
  `location_store.rs` above, specifically because it piggybacks on a structure phase 1
  is building anyway rather than requiring new machinery.
- A shard-per-core runtime redesign (Seastar/Glommio-style) or SPDK-level kernel bypass.
  Not conditional, out of scope regardless of what profiling shows: too large a rewrite
  for 6 days.
- **io_uring is not ruled out, just conditional.** The cheap wins (concurrent fan-out,
  connection pooling, zero-copy `bytes::Bytes` buffers) happen first and are expected to
  cover most of the available speedup. If, after that, profiling shows local disk I/O in
  `local_store.rs` (not network) is the actual bottleneck, swap its file I/O for
  `tokio-uring` (keeps the existing `tokio` runtime, smallest-blast-radius io_uring crate
  available) behind the same `Storage` trait, so nothing above it changes. Only reach for
  this if the phase-1 latency numbers actually show disk I/O dominating. Document the
  profiling result either way.

## Phased build plan

**Default parameters.** Dev-loop iteration (phase 1, steps 1-7) uses a small local
cluster, RS(3,2) (5 processes on different ports), so each correctness/benchmark cycle
is fast. The real experiment (phase 2) switches to RS(9,6), matching the poster's own
simulated parameter, specifically so the real numbers are comparable to the simulator's
without a parameter change confounding the comparison.

**Phase 2 environment:** reusing `openaurora` is an assumption carried over from the
simulator-era work, not yet verified for this purpose — confirm it has a Rust toolchain
and real (non-sandboxed, root-capable) access before relying on it for the loopback+XFS
step; provision fresh GCP VMs instead if it doesn't.

**Benchmarking discipline: every step below ends with a number, not just a "works."**
Before moving to the next step, measure the thing that step just built (latency, or
throughput, or both, whichever is cheap to capture with `tokio::time::Instant` or a
one-line `criterion`/`hyperfine` run) and write the number down. This catches regressions
and bottlenecks as they're introduced instead of only at one checkpoint, and gives every
later claim in the paper a real measurement to point to instead of an assumption.

**Cross-platform: benchmark on both the local Mac dev machine and the Linux GCP VM
wherever a step can run on both.** Dev-loop iteration happens on macOS (this session's
actual environment); the real deployment target is Linux. A number measured only on one
doesn't necessarily transfer (different filesystem, different network stack, different
CPU) — so each applicable benchmark gets recorded on both once the GCP VM exists, not
just once. The one exception is anything Linux-only by construction (loopback devices,
`mkfs.xfs`): there, Mac gets a same-shape substitute benchmark for comparison (e.g. plain
host-filesystem throughput) rather than no number at all.

**Phase 1 (now, move very fast): the core distributed engine.** Target is days, not the
full 6, since content-dependent placement no longer competes for this time:

1. ✅ `cluster` module skeleton: `membership.rs` (static list + `/cluster/join` data
   structures), `PlacementPolicy` trait + `ComputedPlacement` (HRW/rendezvous hashing,
   not modulo), `ec.rs` (`ErasureCoder` trait + `ReedSolomonCoder`), 8 unit tests
   passing (encode/decode round-trip, one-missing-shard reconstruction, too-few-shards
   error, HRW determinism, HRW churn-minimization on peer join, membership add/merge
   idempotence). *Benchmark (RS(9,6), single core, `cargo bench --bench ec_bench`):
   encode 520-550 MiB/s across 1/8/64MB payloads; decode-with-one-missing-shard
   1.75-1.98 GiB/s (faster than encode, since reconstruction reuses the k present shards
   directly rather than recomputing all parity from scratch).*
2. ✅ Per-node storage backing script written (`scripts/setup_xfs_storage.sh`) **and run
   for real** on `openaurora` (GCP, Ubuntu 24.04, 24 vCPU, pd-ssd, started → used →
   stopped per session, per the user's explicit "bring it down after every use"):
   `losetup` + `mkfs.xfs` worked exactly as designed, 5 separate loop-mounted XFS
   volumes, one per node, mounted under 2 minutes total.
   - *First benchmark (`dd ... conv=fsync`, 512MB writes, 3 runs each, repeated twice for
     consistency): loop-mounted XFS 288-291 MB/s vs. host ext4 305-311 MB/s —
     XFS-via-loopback ~6-7% slower than ext4, same `pd-ssd` disk both sides (confirmed via
     `gcloud compute disks describe`, ruling out a disk-tier confound).*
   - *User correctly pushed back: XFS should be faster, not slower — isolate whether
     that ~7% is loopback overhead or XFS itself. Attached a second, fresh `pd-ssd` disk
     (`/dev/sdb`, no loop device), formatted it `ext4` then reformatted the *same* raw
     disk `xfs`, same `dd` test both times: **ext4 315 MB/s vs. XFS 329 MB/s — XFS is
     ~4.4% faster once the loopback indirection is removed.** This fully explains the
     earlier result: a loop device is a file inside the host filesystem, so every write
     to loop-mounted XFS pays two filesystem translations (XFS's own extent allocation,
     then the loop driver's write into its backing file, then the host filesystem's own
     block allocation for that file) — a real, structural cost of the loopback
     technique, not a property of XFS. XFS itself is faster than ext4 here, exactly as
     expected; the deployment technique the plan calls for (loopback, for per-node
     isolation on a shared VM) has its own, now-quantified ~11% combined cost (329→291)
     relative to bare-XFS, which is the honest price of that specific choice.*
   - Mac substitute number from the dev-loop (host filesystem/APFS) was never comparable
     to begin with — different hardware, different filesystem — the Linux numbers above
     are the real ones.
3. ✅ `coordinator.rs` write and read paths, concurrent fan-out over the HTTP `PeerClient`.
   **Corrected during implementation:** uses the existing *native* API (`/put/{key}`,
   `/get/{key}`), not the S3 API — the native API only reads `User`/`Bucket` headers, no
   SigV4 signing needed for internal traffic, which the S3 API would have required.
   Shards are addressed as `{bucket}__{key}__shard{i}` under a reserved
   `User: __cluster__` namespace, FlatBuffers-encoded (reusing the native API's own
   payload format). **Real bug found and fixed in this step:** `location_store.rs` is
   per-node, so a GET landing on a different node than the one that handled the PUT
   couldn't find the object — the pin has to be *replicated* to every shard-holder peer
   at write time (`/cluster/_internal/location`), not just written locally. Fixed and
   verified: a 5-local-process cluster (`RS(3,2)`), PUT through node0, GET from all 5
   nodes returns byte-identical data. *Benchmark (localhost, 5 processes, ~70-byte
   payload, 20 runs each): PUT median 0.90ms (mean 1.00ms, range 0.77-3.18ms); GET median
   0.55ms (mean 0.56ms, range 0.10-1.34ms). This is a tiny-payload/protocol-overhead
   baseline, not a throughput number — the point is it's a real measurement every later
   change (gRPC swap, io_uring) gets compared against, not an assumption.*
4. ✅ **Transport decision point, resolved: gRPC wins, now the default.** Built
   `GrpcPeerClient` (`tonic`+`prost`, `proto/shard.proto`), server side calling a new
   `shard_storage.rs` helper directly (same `StorageService`/`MetadataService` calls the
   native HTTP handlers use, same shard-key convention, so shards written via either
   transport are readable via the other). **Deviated from the original plan text:** the
   shard payload is a plain protobuf `bytes` field, not FlatBuffers-wrapped — protobuf's
   own message framing already provides the structure (bucket/key/shard_idx/data) that
   FlatBuffers would have added, so wrapping it a second time was pure overhead with
   nothing to show for it. **Bug caught before trusting the comparison:** the first
   `GrpcPeerClient` draft opened a fresh connection per call while `HttpPeerClient` reuses
   a pooled client — fixed with a per-peer `Channel` cache so the comparison is apples to
   apples. *Benchmark (`transport_bench`, localhost, single node, connection-reused both
   sides, median of 30-50 iterations):*
   - *4KB: HTTP put 0.250ms / get 0.153ms — gRPC put 0.162ms / get 0.108ms (gRPC ~35% faster both ways).*
   - *256KB: HTTP put 0.466ms / get 0.379ms — gRPC put 0.336ms / get 0.252ms (gRPC ~28% faster).*
   - *2MB: HTTP put 2.508ms / get 2.121ms — gRPC put 2.611ms / get 2.100ms (roughly tied — bulk transfer dominates over protocol overhead at this size).*
   gRPC is now `coordinator.rs`'s default (`WARPDRIVE_PEER_TRANSPORT=http` to revert).
   HTTP implementation kept in the tree, not deleted, per the decision-point design.

   **Third candidate, built and benchmarked: raw TCP + length-prefixed FlatBuffers (no
   HTTP/2 at all) — tried, measured, and rejected as the default, with a real structural
   reason, not just a benchmark loss.** `ShardWire.fbs` (a proper flatc-generated schema,
   committed as `shard_wire_generated.rs` — this project's existing convention,
   `flatc` run once locally, not regenerated at build time) defines a union `Envelope`
   over Put/Get request/response tables; wire framing is a 4-byte length prefix + the
   FlatBuffers bytes, nothing else. `TcpPeerClient` pools plain `TcpStream`s per peer.
   - *Per-call latency (`transport_bench`, no concurrent load): TCP clearly fastest at
     every size — 4KB put 0.116ms vs. gRPC 0.149ms (~22% faster), get 0.076ms vs. 0.097ms
     (~22% faster); 256KB put ~31% faster, get ~23% faster; 2MB roughly tied. This part
     of the hypothesis was correct — stripping HTTP/2 framing does cut per-call latency.*
   - **Real bug caught, same class as the gRPC pool bug:** the first pool implementation
     had no bound — fine on Mac, but under concurrent load, callers that found no idle
     connection each opened a new one, unboundedly. On Mac this just worked (high default
     file descriptor limit). *Decision made from Mac evidence alone (wrong, corrected
     below):* reverted to the unbounded version as "simpler and faster," since a bounded
     version with a semaphore measured slower on Mac and no failure had been observed yet.
   - **That decision broke on GCP.** Same unbounded design, same load test: **`Too many
     open files (os error 24)`**, with 64,054 errors out of ~77k requests at
     concurrency=256. Mac's generous default `ulimit -n` had been masking a real
     correctness bug, not validating a design choice.
   - **First fix (pool size 128, matching the idea of "generous") also failed the same
     way** at concurrency=256 on this 5-node cluster: unlike gRPC, TCP has no
     multiplexing, so pool size directly caps concurrent in-flight requests per peer —
     `peers × pool_size × 2` (outbound + the matching inbound load from peers dialing
     in) has to fit under `ulimit -n` (1024 here) with headroom for everything else the
     process needs. `128 × 4 × 2 ≈ 1024`: no margin at all.
   - **Final, safe default: pool size 32** (`32 × 4 × 2 = 256`, real headroom). Zero
     errors at both concurrency=64 and 256. But: **~8-9x slower than gRPC at this safe
     size** — 781-803 req/s vs. gRPC's 6841-7005 req/s, measured back-to-back on the same
     GCP VM in the same session. This is the real, structural tradeoff: gRPC's HTTP/2
     multiplexes hundreds of concurrent logical requests over a handful of connections;
     raw TCP needs one connection per in-flight request, so its *safe* concurrency
     ceiling is directly bounded by file descriptors — there is no pool size that is
     both fd-safe and fast without reinventing multiplexing, which defeats the point of
     trying a simpler transport in the first place.
   - **Conclusion: gRPC stays the default.** TCP is fully correct, tested (unit tests +
     multi-node correctness + EC reconstruction, all passing), and kept in the tree —
     `WARPDRIVE_PEER_TRANSPORT=tcp` to use it — as a documented "no" with real numbers
     and a structural reason behind it, not an unexplored idea. The per-call latency win
     is real but never materializes under concurrent load, which is the regime that
     actually matters for this system.
   - **Methodology lesson, stated plainly because it cost real time twice in one
     session:** a resource-limit difference between dev (Mac) and target (Linux cloud VM)
     environments can completely invert a benchmark conclusion. Decisions about
     concurrency-sensitive resource pooling need verification on the actual target
     platform, not just the fast dev-loop one — exactly why this plan's own cross-platform
     benchmarking discipline exists, and a case where skipping it would have shipped a
     transport that crashes in production.

   **Final decision for this phase: gRPC is the transport, full stop. Raw TCP +
   FlatBuffers is parked, not abandoned.** It isn't wrong — it's correct, tested, and its
   per-call latency advantage is real — it's just structurally unable to win under
   concurrent load without multiplexing, which is real additional engineering (pipelining
   multiple in-flight requests per connection, or framing multiple logical messages onto
   fewer sockets) that would start to re-create what gRPC already does. Worth revisiting
   only if a concrete, specific reason shows up later (e.g. gRPC/HTTP2 itself becomes the
   bottleneck again at a scale this session didn't test, or a deployment target has a
   much higher `ulimit -n` ceiling where the fd-vs-throughput tradeoff looks different).
   Not a to-do for this plan right now.
5. ✅ **Real gap found and fixed: `Membership::merge()` existed but was never called.**
   The `/cluster/join` *handler* (receiving side) was built and tested in isolation, but
   nothing ever called it or merged its response — a new node had no way to learn about
   the cluster, and the design doc's "converges via one broadcast round" was aspirational,
   not implemented. Added `join_cluster_via(bootstrap, self_addr, membership)`
   (`membership.rs`): announces this node to the bootstrap peer, merges the returned
   list, then announces itself to every other peer in that list too — convergence comes
   from the new node broadcasting, not from gossip among the existing ones. Wired to
   `WARPDRIVE_JOIN_VIA`/`WARPDRIVE_SELF_ADDR` env vars, spawned at startup.
   *Verified end to end (6th node joining a running 5-node local cluster): exactly 5
   `POST /cluster/join` calls logged (1 to the bootstrap, 4 broadcast to the rest); the
   new node immediately succeeded as coordinator for a fresh PUT (confirms it actually
   learned the full peer list, not just enough to not error); the pre-join object read
   back byte-identical afterward (confirms the placement-pin design's core promise:
   joining never moves or breaks existing data).*
6. ✅ Correctness pass, done early (opportunistic, same local cluster was already up):
   PUT/GET round-trip byte-identical across all 5 nodes (step 3's test). Killed node1
   (`kill -9`), GET from node0 for the same object still succeeded via EC reconstruction
   in 2.2ms, byte-identical — real evidence the quorum/reconstruction path works, not
   just unit-tested in isolation. Also verified object lock end to end: set `COMPLIANCE`
   retention on one node, confirmed `DELETE` from a *different* node (port 9712) was
   rejected (403) while the object was still locked, and that an unlocked object on the
   same cluster deletes normally (200) — the no-distributed-lock-manager design from
   `location_store.rs` works as described. *Benchmark: reconstruction GET 2.2ms (one
   peer down, 4/5 shards available, k=3 needed). Direct (no reconstruction) GET median
   was 0.55ms per step 3 — reconstruction cost here is ~4x, though at this tiny payload
   size that's dominated by RPC overhead, not real GF math; revisit at a larger payload
   once phase 2 has real data to push through.*
7. ✅ Concurrent-vs-sequential fan-out (`fanout_bench`, 5 real peers, 4KB shards, 30
   iterations): concurrent (`join_all`) median 0.513ms vs. sequential (one at a time)
   median 0.978ms — **1.91x speedup**, confirming the one speed lever in scope actually
   works. This is a localhost floor: real RTT between separate VMs in phase 2 should push
   the speedup closer to Nx (5x for 5 peers), since sequential there sums real network
   RTTs instead of near-zero loopback latency.

   **Resource-saturation check (user-requested, done with a sustained load generator, not
   a curl loop — 200 backgrounded curl processes finished before `top` could sample them,
   measuring fork/exec overhead instead of the server).** Built `load_gen` (64 concurrent
   in-flight PUTs, 8s, 4KB payload) against the 5-node local cluster, sampling per-process
   CPU with `top -pid` once load was steady-state:
   - Coordinator (node handling the fan-out): 190-222% CPU (1.9-2.2 cores) — real
     concurrent work (EC encode + orchestration), not serialized onto one core.
   - Each of the 4 peers: ~90-104% CPU (~1 core each) handling inbound gRPC shard writes.
   - System-wide: ~90-95% CPU utilized (34% user / 58-63% sys / <10% idle), 2757 req/s
     sustained, **zero errors**.
   - **Documented, not fixed (per the project's own "document bottlenecks and move on"
     rule):** sys-time is unusually high (58-63% of total CPU) relative to user-time,
     suggesting per-request cost here is dominated by syscalls (socket I/O across 5 OS
     processes × many concurrent HTTP/2 and HTTP/1.1 streams) rather than useful
     computation. Worth a flamegraph if phase 2's real-VM numbers show the same pattern.

   **Repeated on real Linux hardware (`openaurora`, 24 vCPU GCP VM, `perf stat`, started
   → used → stopped within one session) — and the result changed the story.** The same
   5-node cluster, real loop-mounted XFS storage, `perf stat -p <pids>` attached during
   a sustained `load_gen` burst:
   - At concurrency=64: only ~3.65 of 24 cores busy (`task-clock` 29.2s of CPU time over
     8.0s wall time across all 5 processes), throughput 982 req/s.
   - At concurrency=256 (4x more clients): throughput and CPU both stayed **flat**
     (977 req/s, ~3.3 cores busy) — and 20 errors appeared. Increasing concurrency did
     not increase throughput at all, on a machine with plenty of idle cores to give it.
   - This is the opposite finding from the Mac run (which *did* scale with load, up to
     ~90% system CPU). The GCP number means the bottleneck here isn't CPU count or
     client concurrency — something in the pipeline serializes throughput regardless.
     Context-switches were high (~29-32k/sec) and `cpu-migrations` were high
     (~35-41k/sec) on both runs, consistent with threads frequently blocking rather than
     doing parallel work. Hardware counters (`cycles`/`instructions`/`cache-misses`)
     reported `<not supported>` — this GCP VM type doesn't expose a virtual PMU, so a
     cycle/IPC-level breakdown isn't possible here; only software events are.
   - **Best-evidenced hypothesis, not yet confirmed (would need a flamegraph to nail
     down, which didn't fit in this session's VM time):** shard *bytes* still go through
     the existing single-node `MetadataService`/SQLite stack on each peer (reused by
     design via the native API, per `shard_storage.rs`) — which has the same
     single-writer characteristic the user flagged earlier this session as a real
     bottleneck. `location_store.rs` (Bitcask) already fixed this for the *placement
     pin*; the shard-byte path was deliberately left on the existing, tested stack and
     may need the same treatment if phase 2's real numbers confirm this hypothesis.
     Documented as a likely next bottleneck, not fixed now — consistent with the
     project's own "document bottlenecks and move on" rule, and worth a real flamegraph
     in phase 2 before deciding whether to act on it.

   **Hypothesis confirmed and fixed, same session.** The user pushed directly on this
   ("how are we still using SQLite and still be distributed?") — a fair challenge, since
   the whole point of `location_store.rs` was to get SQLite out of the cluster path, and
   it had only done that for the placement pin, not the shard bytes. Checked
   `metadata/sqlite_store.rs` precisely: one process-wide `lazy_static
   Arc<Mutex<Connection>>` — every metadata call in a process serializes through that
   single mutex, exactly matching the flat-throughput symptom. Fix: added
   `shard_meta_store.rs`, the same Bitcask pattern as `location_store.rs`, applied one
   layer deeper (shard_key → (offset, size) instead of key → peer list); `shard_storage.rs`
   now calls the `Storage` trait directly and this new store, never touching
   `MetadataService`/SQLite. **Note:** this only fixes the gRPC transport's server side
   (now the default) — `HttpPeerClient`'s server side is still the unmodified native
   API, which still goes through SQLite; documented as a real, understood difference
   between the two transports now, not just a latency gap.
   - *Local (Mac) re-check, same hardware, same concurrency=64: 2757 → **4987 req/s**
     (~1.8x).*
   - *GCP re-check (`openaurora`, same 5-node cluster, real loop-mounted XFS storage):
     concurrency=64 982 → **2787 req/s** (~2.8x). Still plateaus going to concurrency=256
     (2709 req/s) — so SQLite's mutex was a major bottleneck, confirmed by fixing it, but
     not the only one.*
   - *Filesystem isolation, same comparison again but on real raw-XFS partitions
     (`/dev/sdb1-5`, GPT-partitioned, no loop device — the user asked directly whether
     we'd checked this): **2622 req/s at concurrency=64, 2708 req/s at concurrency=256**
     — essentially identical to the loop-mounted-XFS numbers above, and still well below
     the Mac's 4987 req/s at the same concurrency. This is the key finding: the ~7%
     raw-disk cost of loopback-vs-bare-XFS measured with `dd` (see storage-backing step
     above) **does not show up at all** at the application level — it's completely
     swamped by whatever causes the flat ~2700 req/s ceiling. Disk I/O isn't the
     remaining bottleneck for small (4KB) shard writes; filesystem choice was a red
     herring for closing the Mac-vs-GCP throughput gap specifically.*

   **A second real bottleneck found and fixed along the way, in `local_store.rs`
   itself.** Before localizing the gRPC connection issue below, found that `write()`
   held one process-wide `Mutex<()>` across the *entire* operation (open, seek, write,
   flush) — every write in a process, for every bucket, fully serialized, regardless of
   available cores. Fixed with a per-`(user, bucket)` atomic next-offset counter plus
   `write_at` (positioned write, no shared cursor) — concurrent writes to different
   buckets stop blocking each other entirely, and concurrent writes to the *same*
   bucket stay correct via `fetch_add` instead of a lock held for the whole operation.
   Added a dedicated test (16 threads × 50 writes to one bucket, verifying no
   overlapping extents and byte-exact reads) since this touches shared single-node
   code, not just new cluster-only code. **Honest result: this fix alone did not move
   `shard_fanout` latency at all** (Mac re-check: 2757 → 4987 req/s from the *other* fix
   that session, this one's isolated effect was unmeasurable against it) — it was a
   real, necessary fix for write concurrency in general, just not the explanation for
   *this* specific flat-throughput symptom. That's what motivated localizing further
   below rather than stopping here.

   **The remaining ceiling, actually diagnosed (the user pushed for this specifically:
   "figure out where we are slow if we are not saturating anywhere").** Added real
   phase-level timing (atomics, not per-request logging, so measuring doesn't itself
   perturb the measurement): `encode` / `shard_fanout` / `location_replicate` on the
   coordinator, `storage_write` / `meta_put` on the server side of a shard write, and
   `channel_lookup` / `rpc_call` inside the gRPC client. Localized precisely:
   - `shard_fanout` dominated (~10ms of ~11.7ms total) — but server-side `storage_write`
     + `meta_put` together were only **~0.07ms**. The ~10ms wasn't application work at
     all.
   - Within the client, `channel_lookup` was negligible (~0.007ms); `rpc_call` itself was
     ~6.8ms. So the cost was the RPC round trip, not cache lookup, not the handler.
   - **Root cause**: each peer had exactly one cached, shared gRPC `Channel` (one HTTP/2
     connection). hyper/h2 drives a connection's multiplexing through one internal task
     per connection — real concurrent streams, but funneled through one task. At 64+
     concurrent requests × 5 peers, that one task per peer became the serialization
     point: low CPU (threads waiting on the connection, not computing), flat throughput
     regardless of client concurrency (more clients just means more streams queued on
     the same 5 connections) — exactly the symptom observed on GCP, and consistent with
     Mac scaling fine (its single-core performance made that one task's work cheap
     enough not to matter yet at this concurrency; GCP's `e2` family vCPUs are
     cost-optimized, not performance-optimized per vCPU, so the same serialized work
     costs more there).
   - **Fix, with a real bug caught along the way:** added a small pool of `Channel`s per
     peer (`WARPDRIVE_GRPC_POOL_SIZE`, default 8), round-robin. The *first* version used
     a plain `Mutex<HashMap<_, Arc<_>>>` with a check-then-build pattern — under real
     concurrent load (GCP, 256 clients × 5 peers), many callers simultaneously saw "no
     pool yet" and each independently opened 8 connections: a thundering herd of
     thousands of simultaneous connection attempts that **collapsed throughput to 57.6
     req/s with 1057 errors** (from a baseline of ~2787 req/s) — a real regression,
     caught by testing on GCP specifically, where connection overhead is non-trivial
     (Mac's cheap loopback never exposed it). Fixed with `tokio::sync::OnceCell` per
     endpoint: exactly one caller builds a given peer's pool; every other concurrent
     caller awaits that same in-progress build instead of starting its own.
   - **Re-verified on GCP after the real fix**: concurrency=64 → **3411 req/s** (vs.
     2787 before pooling); concurrency=256 → **3939-5530 req/s across runs**, throughput
     now *increasing* with concurrency instead of flat or collapsing, ~0 errors. `perf
     stat` during a 256-concurrency run: **~13.1 of 24 cores busy** (task-clock
     104.9s / 8.0s wall), up from ~3.3-3.65 cores before any of these fixes — direct,
     measured confirmation that the system is now actually using the hardware it has,
     which is what "saturating" means and was the original open question.
8. ✅ **io_uring decision point: skip it, disk I/O is negligible.** The phase-level timing
   added earlier for the gRPC connection diagnosis already answers this directly — no new
   instrumentation needed, just reading what was already being measured. Clean run
   (Mac, 5-node cluster, gRPC, concurrency=64, 4KB payload, `n=34902`):
   - Client-side total request latency: **13.839ms avg**.
   - Server-side disk I/O (`storage_write` + `meta_put` combined, all 5 nodes): **~0.13ms
     avg**, remarkably consistent node to node (0.129-0.135ms).
   - Disk I/O is **~1% of total latency**. The other ~99% is `shard_fanout` +
     `location_replicate` (network/RPC), already diagnosed and addressed in step 7.
   - Per the plan's own stated criterion ("if disk I/O is a minor fraction of the total,
     skip `tokio-uring` and document that finding"): **skip it.** `local_store.rs` stays
     as is. Revisit only if a future profiling pass on different hardware or a different
     (larger, disk-bound) workload shows a different ratio — nothing in this session's
     data suggests that's likely.
   - *Minor aside, not the decision driver:* `storage_write_max_ms` occasionally spiked to
     ~119-120ms (rare tail, not the average) — consistent with OS scheduling jitter under
     load, not investigated further since it doesn't change the "skip io_uring" conclusion
     either way.

**Phase 1 complete.** All 8 steps done, each with a real measurement, not an assumption —
including two self-corrected mistakes (the gRPC thundering-herd bug, the TCP fd-exhaustion
bug) that real cross-platform testing caught before they could ship.

**Phase 2 (after phase 1 is solid): real multi-VM deployment and the actual experiment.**
Only start this once phase 1 is correct and reasonably fast. Run the existing two-workload
granularity/selectivity sweep against the real cluster, replacing the simulator's numbers.

- ✅ **Infrastructure baseline, done.** 5 separate `e2-small` VMs (`wd-node0`-`4`, pd-ssd,
  genuinely separate machines on the default VPC, not processes on one VM) — this
  session's **first real network-hop test**: PUT via `wd-node0`, GET via `wd-node4`,
  byte-identical, confirming the engine works over real inter-VM latency, not just
  loopback. *Benchmark (`load_gen`, concurrency=16, 4KB payload, `RS(3,2)`,
  `ComputedPlacement`): 1389.6 req/s, zero errors.* Lower than the single-VM
  many-process numbers from phase 1 (expected and correct: real network RTT instead of
  loopback, and `e2-small` is 2 vCPU vs. `openaurora`'s 24) — this is the first true
  apples-to-apples "real distributed, real network" number for the project.
- **Build note:** compiling Rust on the `e2-small` nodes directly was painfully slow
  (2 vCPU, single-core-bound dependency compilation) — switched to building once on
  `openaurora` (24 vCPU, already set up) and distributing the resulting binary to all 5
  small nodes instead. Faster, and these are the same OS/arch so the binary runs
  unmodified. `server_log.yaml` has to ship alongside the binary — `main.rs` calls
  `log4rs::init_file` relative to CWD and panics without it; caught this via one node
  crashing silently until checked.
- **Important scope clarification (user caught this directly):** this baseline uses
  `ComputedPlacement`, which has no concept of Parquet column chunks, IVF partitions, or
  a granularity knob — it cannot reproduce the poster's actual Findings 1-3 (which are
  specifically about content-dependent placement's benefit). This baseline proves the
  distributed engine itself is correct and real; **Phase 3 (below) is what's actually
  needed** to replace the poster's simulated granularity-knob numbers with real ones.
- **Not yet done:** porting the real Parquet/IVF workload generators
  (`hipc_poster/formats.py`) to drive traffic through the `/cluster/` API, and a possible
  one-time MinIO comparison run on the same real infrastructure (user's suggestion —
  strengthens the poster with a real third-party baseline, not just simulator-vs-real).

**Phase 3 (content-dependent placement, later, time permitting):** only after phases 1-2
are done and only if days remain before Oct 8 AOE. Implement `ContentDependentPlacement`
behind the trait boundary built in phase 1: port `fac_core.py`'s `construct_stripes`,
add the `x-warpd-computable-units` header, add the per-bucket granularity knob. If this
phase doesn't fit, the poster reports phase 1+2 results honestly (a real, fast, distributed,
erasure-coded core with one placement policy) and states content-dependent placement as
designed-for-but-not-yet-implemented, which is itself a defensible, honest claim.

This plan intentionally does not assign calendar days to phases 2-3 yet — phase 1's actual
velocity will tell us how much time is left, which is the point of moving very fast on it
first.

## Verification

- Unit tests for `ec.rs` (encode/decode round-trip, including one shard missing),
  `placement.rs` (`ComputedPlacement` determinism: same `(bucket, key)` always maps to
  the same peer set), and `location_store.rs` (write a record, kill the process, restart,
  confirm the in-memory index rebuilds correctly from the log replay).
- Quick write-throughput comparison of `location_store.rs` vs. the existing SQLite layer
  at a representative record rate, since the whole point of building it was SQLite being
  a bottleneck — have the number that justifies the new store, not just the intuition.
- End-to-end: PUT an object through the cluster, confirm it reads back byte-identical.
- PUT an object, then join a new node via `/cluster/join`, then GET the same object:
  confirm it still resolves via its `location_store.rs` pin, not a fresh (and now
  different) `ComputedPlacement` recompute.
- PUT an object with `retention_mode: COMPLIANCE` and a future `retain_until`, confirm
  DELETE is rejected until that date; confirm a GOVERNANCE-mode lock can be overridden
  with the appropriate permission and a COMPLIANCE-mode one cannot, by any peer.
- Kill one peer process, confirm `GET` still succeeds via EC reconstruction (one-time
  correctness check, not a steady-state benchmark per the non-goals above).
- Concurrent vs. sequential fan-out latency comparison, as the concrete evidence for the
  one speed claim this phase makes.
- Once phase 2 runs: real numbers should land in the same order of magnitude as the
  simulator's where comparable (sanity check), with any divergence explained in the paper
  rather than hidden.
