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

## North star (locked in, 2026-10-02)

Everything in this document — already built and still to come — is in service of
three goals, in this priority order. This section is the one future work gets
checked against; a feature that doesn't serve one of these three doesn't belong
here regardless of how interesting it is.

> "I basically want three things. Fast distributed object store by default,
> explainability of our whole stack for SLAs and customisability with explaining
> cost of operations for that particular customisation to still keep it running
> without faults. So you can lock in this direction for us."

**1. Fast, distributed, erasure-coded object storage by default.** Nobody has to
opt into performance or into distribution — a bucket with zero configuration gets
`ComputedPlacement` (rendezvous hashing) and `ReedSolomonCoder`, concurrent
fan-out, and MinIO's own quorum rule, out of the box. Phase 1/2's own numbers
(1389.6 req/s @ concurrency 16, zero errors, on real multi-VM GCP hardware) are
the evidence this is true today, not an aspiration. Customization (point 3) is
additive on top of this baseline, never a prerequisite for it.

**Checked directly, 2026-10-02: do phases 3/4's additions cost the plain path
anything?** Built phase 1/2's own commit (`a3951b2`, before content-dependent
placement, bucket config, or pushdown existed at all) in a separate `git
worktree` — no stash tricks, no risk to the working tree — and ran the exact
same `load_gen` methodology (concurrency 16, 4KB payload, RS(3,2), local
5-node cluster, two runs each) against it and against the current tree, back
to back on the same machine. `load_gen` sends no `x-warpd-computable-units`
header, so this measures precisely the thing at risk: whether the new
header-check-and-dispatch costs anything on the path that never uses it.
Phase 1/2 baseline: 4537.1 and 4686.1 req/s (avg 4611.6). Current: 4686.1 and
4493.6 req/s (avg 4589.9). No measurable regression — the spread between the
two builds is smaller than the run-to-run noise on either one. This makes
sense structurally, not just empirically: the dispatch only does a header
lookup (checking for absence, the common case) before falling straight
through to the same plain-path code, now named `put_object_plain` but
otherwise unchanged. Every future extension should keep this property —
gated behind an explicit opt-in signal (a header, a bucket config entry),
never evaluated unconditionally on the default path — and should get the same
kind of before/after check before being called done, not just an assertion
that it's probably fine.

**2. Explainability of the whole stack, sufficient to state an SLA.** Not "the
code is readable" — every operation the system performs has to produce a real,
inspectable number an operator could put in a contract: storage overhead vs.
optimal (`overhead_pct`, computed from what the live packer actually did, not a
separate model), write/read quorum semantics (`required_write_acks`, documented
and tested against MinIO's own published rule), per-phase latency breakdowns
(`cluster_timing_stats`, `shard_server_timing`, `grpc_client_timing`), and the
literal packing/placement record behind any object (`content_record`). An SLA is
a promise backed by a number the system itself can produce on demand — this is
why every new mechanism in this project ships with a diagnostic endpoint, not as
an afterthought but as part of what "done" means.

**3. Customizability that explains its own cost and cannot take the system down.**
Two separate commitments, and both are required, not either/or:
- *Explains its own cost*: swapping in a different `StripePacker` (or, later, a
  different `PlacementPolicy`/`ErasureCoder`) must come with a real, computed
  answer to "what does choosing this cost me" — today that's `overhead_pct`
  for packing; the same expectation applies to whatever trait gets a registry
  next (see the open item below).
- *Cannot take the system down*: a customer's own placement/packing choice is
  never allowed to compromise durability, availability, or liveness. The
  per-bucket overhead-threshold fallback (`bucket_config.rs`, mirroring Fusion's
  own mechanism) is the first concrete instance of this pattern — an
  over-threshold or misconfigured custom packer degrades to the plain,
  always-safe path with a logged reason, never a failed write and never silent
  data loss. This is the pattern to repeat for every future customization point:
  a safe, explainable default behind every pluggable choice, not a trapdoor a
  bad customization can fall through.

**How customization eventually ships (locked in, 2026-10-02):**

> "tomorrow users should probably ship the code in the UI we expose or something
> and we should be able to simulate the correctness etc and give them stats."

So the end state for pillar 3 isn't "a developer registers a new Rust struct
and redeploys" (today's mechanism) — it's a customer submitting their own
placement/packing logic through a UI WarpDrive exposes, with WarpDrive itself
validating and pricing it before it ever runs against real data:

1. **Submit.** A customer provides their own `StripePacker` (and eventually
   `PlacementPolicy`/`ErasureCoder`/`ColumnCodec`) implementation through a
   UI, not a PR to this repo.
2. **Simulate for correctness.** WarpDrive runs the submission against the
   same class of property the unit tests in `packing.rs` already check for
   `FacPacker` — never splits a unit, every unit appears exactly once, no bin
   exceeds its stripe's capacity — against synthetic and/or replayed
   workloads, before the submission is allowed near real data.
3. **Report stats.** The same way `overhead_pct` is computed from what a real
   pack run actually produced, the simulation reports the customer's real
   cost for their own choice (overhead %, and eventually latency/availability
   impact) — pillar 3's "explains its own cost," now at the product surface
   a customer actually sees, not just an internal diagnostic endpoint.
4. **Gate activation.** Only a submission that passes step 2 and reports an
   acceptable cost in step 3 is eligible to go live on a bucket — the
   `bucket_config.rs` overhead-threshold fallback is the mechanism that keeps
   this safe at runtime even after activation, not a substitute for
   validating before activation.

**Execution model (locked in, 2026-10-02): WASM.** A customer's submission runs
as a WASM module (wasmtime/wasmer), loaded and invoked by the already-running
coordinator process — no restart, no redeploy, no recompiling WarpDrive itself,
which matters specifically because this is a managed service and can't go down
to pick up one customer's code. A `WasmPacker` wraps the loaded module behind
the existing `StripePacker` trait, so from `coordinator.rs`'s point of view a
WASM-backed packer is just another registry entry, no different from
`FacPacker`. Resource-limited (CPU/memory budget, no syscalls, no network, no
filesystem — the module can't touch anything but the bytes it's handed), the
same sandboxing model Cloudflare Workers and Shopify Functions use to run many
different customers' code inside one always-on process. This is the execution
*substrate*, not the authoring experience — two front ends sit on top of it,
both compiling down to the same sandboxed module:
1. **DSL / plain-language input**, for most customers: an LLM translates a
   constrained description of the desired policy into real code, which is then
   compiled to WASM. Most of a DSL's usual expressiveness ceiling goes away
   once an LLM is the one composing it, without giving up the sandbox.
2. **Direct `.wasm` upload**, for a customer who wants to hand-write something
   as involved as Fusion's own algorithm themselves.
Both land in the same runtime, the same resource limits, the same `WasmPacker`
wrapper, and the same simulate-and-report-stats gate (steps 2-3 above) before
anything goes live. "Accept a FaaS model" and "use WASM" are the same decision
at two layers, not two competing ones: FaaS describes the product shape
(upload a function, invoke it on demand, meter it); WASM is what actually runs
it safely underneath, the way Cloudflare Workers is a FaaS product built on
WASM-style isolates. The simulate-and-report-stats harness (steps 2-3) doesn't
depend on this decision and is buildable independently.

**Open implication, not yet done:** today `StripePacker` and `ColumnCodec` are
per-bucket/per-unit registries (pillar 3's "customizable" half is real for
these two); `PlacementPolicy` and `ErasureCoder` are still single global
instances (pillar 3's "explains its own cost, can't take the system down"
guarantees don't yet extend to them, because there's nothing to switch between
per bucket yet). Promoting those two to registries — with the same
overhead/cost-style guardrail pattern — is the direct next step if per-bucket
control over *where* shards land or *how* they're coded is requested, not a
separate idea.

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

**Phase 3 (content-dependent placement): core write/read path done and verified.**
Prioritized ahead of the MinIO comparison per explicit user direction — this is the
poster's actual scientific claim (Findings 1-3), the baseline alone can't reproduce it.

- **Architectural correction the user caught before implementation started:** the first
  instinct was to hardcode `construct_stripes` as a generic bin-packer. Corrected to a
  contract first: `packing.rs`'s `StripePacker` trait (`pack(k, units) -> Vec<Stripe>`),
  with `FacPacker` (Fusion's Algorithm 1, generalized, faithfully ported from this
  project's own `fac_core.py`) as the one shipped implementation — consistent with every
  other component here (`Storage`, `PlacementPolicy`, `LocationStore`, `ErasureCoder`,
  `PeerClient`) being a trait with one concrete implementation, not a concrete type.
  Verified against a hand-traced reference run of `fac_core.py`'s own algorithm, not just
  invariant checks (no unit split, no bin over capacity) — those alone wouldn't catch a
  subtly wrong port.
- **`ErasureCoder` extended, not replaced:** added `encode_shards`/`decode_shards` (given
  `k` pre-chunked, already-content-packed bins, compute/reconstruct `m` parity shards) —
  the existing `encode`/`decode` (flat-buffer split) stay for `ComputedPlacement`'s
  single-stripe path, unchanged.
- **New multi-stripe record + store** (`content_location_store.rs`, same Bitcask pattern
  as `location_store.rs`, kept separate rather than overloading one schema to cover both
  shapes): per object, a `k`/`m`, `original_len`, a unit_id→(offset,len) index into the
  *original* bytes, and one `StripeRecord` (peers, capacity, bin→unit_ids) per stripe.
- **New orchestration** (`packed.rs`): PUT slices the body by the `x-warpd-computable-units`
  header (`[[offset,len],...]`, the poster's own spec — presence of the header is the
  dispatch signal, no separate bucket-config call, and no Parquet/IVF-specific logic
  anywhere in this code, matching `fac_core.py`'s own "no format-specific logic lives
  here"), packs via `StripePacker`, erasure-codes each stripe independently, places each
  stripe's peers by calling the *existing* `PlacementPolicy` once per stripe (varying the
  key, not a new trait impl — `ContentDependentPlacement` never needed to force-fit the
  single-peer-set `PlacementPolicy` signature). GET does the reverse: per stripe, fetch
  shards, `decode_shards`, then walk each bin's unit list to place recovered bytes back
  at their *original* offsets.
- **Verified end to end, cross-node, on the local 5-node cluster:** a 160-byte object (6
  units of distinguishable content, sizes matching the same hand-traced packing
  scenario) PUT through node0 with the header, GET back byte-identical from **all 5
  nodes** — confirming replication works too (added after first catching that the
  content-location pin wasn't replicated at all, same class of bug as the original
  `location_store.rs` gap from phase 1, fixed the same way: reused
  `cluster_internal_put_location`'s exact pattern for a new `content_location`
  endpoint, replicated to the union of every stripe's peers since different stripes can
  land on different peer subsets).
- **Not yet done:** porting the real Parquet/IVF workload generators (`hipc_poster/formats.py`)
  to drive real traffic with real headers against this path, replacing the simulator's
  numbers for Findings 1-3; the per-bucket granularity knob (client-side concern per the
  poster's own framing — granularity is decided when the client builds its
  computable-units list, not inside WarpDrive); and a possible MinIO comparison.

This plan intentionally does not assign calendar days to phases 2-3 yet — phase 1's actual
velocity will tell us how much time is left, which is the point of moving very fast on it
first.

## Phase 4: per-bucket placement config, overhead-threshold fallback, and query pushdown

Phase 3 proved content-dependent placement works end-to-end with real workload
shapes, but left two things hardcoded that the real Fusion paper (ASPLOS'25) treats
as load-bearing decisions, not implementation details: (1) FAC's own stripe
construction has a configurable storage-overhead ceiling, above which it falls back
to plain fixed-block erasure coding rather than accepting arbitrarily bad packing;
(2) the entire second half of the paper — the reason it's about query pushdown at
all — is a filter/projection execution layer on top of placement, which phase 3
didn't touch.

**Per-bucket placement config (`bucket_config.rs`).** Fusion's own paper (§4.2,
§6, Configuration): *"We introduce a system-level hyperparameter in Fusion,
allowing users to specify the maximum additional storage overhead they can
tolerate compared to the optimal. If the algorithm cannot construct stripes
within the specified storage budget, it defaults to erasure coding the object
into fixed-sized blocks... We set the storage overhead threshold to 2% in
Fusion."* This is a real, paper-sourced mechanism, not an invented one — confirmed by
reading the actual PDF, not assumed from the poster's own files (which don't
mention it at all). We implement the same mechanism, but **per-bucket** instead
of a single global constant, matching how every other bucket-level setting in
this project works (versioning, ACL, object-lock retention): `BucketPlacementConfig
{ bucket, packer_name, overhead_threshold_pct }`, Bitcask-logged, replicated
synchronously to *every* known peer with all-acks required (not quorum — a
placement *policy* disagreement between coordinators is a correctness hazard in a
way a missed shard replica isn't). `ClusterState.packers` is now a
`HashMap<String, Arc<dyn StripePacker>>` registry (was a single `Arc<dyn
StripePacker>`), so a bucket names which packer to use by string key — the literal
mechanism by which "a third, user-defined content-dependent placement policy" only
needs to implement `StripePacker` and be registered, never touching `coordinator.rs`.
Dispatch in `cluster_put_object`: parse the header, look up the bucket's config
(default: `packer_name: "fac"`, `overhead_threshold_pct: 2.0`, matching Fusion's own
evaluation default), pack once, compute `packing::overhead_pct` against the *real*
stripe result, and only commit the content-dependent path if under threshold —
otherwise fall through to the plain path, logged, not an error. An unknown
`packer_name` degrades the same way: a bad config value is never the reason a PUT
fails.

**Verified**: PUT identical row-group-batched data (19.99% overhead, phase 3's own
measured number) into a bucket with no config — falls back to plain EC, confirmed
via the node's own log line and a 404 from `content_record`. PUT the same data into
a bucket configured with a 25% threshold — content-dependent placement used, 4
stripes, `content_record` fetched correctly from a *different* node than the
coordinator that wrote it (replication still correct under the new `UnitMeta`-based
record shape).

**Query pushdown (`pushdown.rs`).** The mechanism, read directly from the paper:
a coordinator decomposes a query into per-column-chunk operations; a filter stage
runs the predicate in-situ on whichever single node holds that chunk's data block
and returns a bitmap; a projection stage decides, per chunk, whether to push the
projection down too, using the paper's own Cost Equation — push down only when
`selectivity × compressibility < 1`, i.e., only when shipping the small filtered
result is actually cheaper than shipping the whole compressed chunk for the
coordinator to decode itself. This works *because* `ErasureCoder::encode_shards`
is systematic Reed-Solomon: shard indices `0..k` are stored as the literal,
unmodified plaintext bins (only `k..k+m` are coded parity) — so the peer holding a
data shard already has real, decoded bytes on local disk, exactly matching Fusion's
claim that pushdown avoids cross-node reassembly for data blocks.

Scope, per the explicit decision to start with the microbenchmark only (not
Q1–Q4's multi-predicate/aggregate queries): filter and projection collapse into
one round trip, since the microbenchmark's query (`SELECT column FROM lineitem
WHERE column < value`) targets the same column for both. `ColumnCodec` is a new
trait (`StripePacker`/`ErasureCoder`-style: one contract, one real implementation)
— `ZlibF64Codec` decodes a little-endian f64 array compressed with zlib/DEFLATE.
This is **not** literally Parquet's own encoding (dictionary + bit-packing +
Snappy) — it's a real, standard, genuinely-decodable stand-in chosen specifically
so the Python workload driver needs no dependency beyond the stdlib `zlib` module
(same reasoning that dropped `requests` for `urllib` earlier this session). A real
Parquet-page `ColumnCodec` is a second implementation away, not a rewrite of
anything that calls this trait — directly relevant to "eventually support S3
Tables," which is real Parquet/Iceberg data these same contracts should decode
without changing `pushdown.rs`'s orchestration.

`x-warpd-computable-units` grew an optional 3rd/4th element per unit —
`[offset, len, uncompressed_len, codec]` — backward compatible with the plain
`[offset, len]` form (defaults to `codec: "opaque"`, compressibility 1.0, not
pushdown-capable, used by every other workload so far). New endpoints:
`POST /cluster/{bucket}/{key}/query` (coordinator-side: look up the record,
locate which single peer holds the requested unit, forward one request) and
`POST /cluster/_internal/pushdown_filter` (peer-local: read the local shard,
decode, filter, apply the cost equation, respond) — symmetric, any node can
receive either.

**Verified end-to-end** on a real local cluster, queried from nodes *different*
from both the PUT coordinator and the unit's owning peer: a highly-compressible,
high-selectivity column correctly disabled projection pushdown (matching the
paper's own documented Q4 case: *"the fare column has a high compression ratio of
152... leading Fusion to disable the projection pushdown"*); a poorly-compressible,
low-selectivity column correctly enabled it, with returned values verified against
ground truth. One known cosmetic issue: 2 of 2000 pushed-down f64 values differed
from the source by 1-2 ULPs after a JSON round trip — a float-serialization
precision artifact, not a filtering/decoding correctness bug (every `matched_count`
across every test matched ground truth exactly).

**`pushdown_benchmark.py`** (hipc_poster/) reproduces Figure 13's shape: a 16-column
synthetic object (cardinality swept from 2 to 20,000, giving real compression
ratios from ~2.9x to ~608x, the same spread Figure 6 reports for real TPC-H
lineitem columns), baseline path = the existing full-object GET (reassembles
across every stripe peer, EC-decodes, client slices+decodes+filters one column)
vs. pushdown path = the new query endpoint. Real measured result on this local,
single-machine 5-process cluster (loopback, not real network — absolute latencies
are far smaller than the paper's real-datacenter numbers, so only the *relative*
reduction is comparable): **63–76% median latency reduction and 60–73% p99
reduction across all 16 columns**, every single query's `matched_count` asserted
equal between both paths. This lands in the same range as the paper's own
headline 64%/81% median/tail — a real reproduction of the *mechanism and its
shape*, not a claim of matching their exact datacenter-scale numbers.

**Not yet done**, honestly: Q1–Q4's multi-predicate filter stage and COUNT/AVG
aggregate pushdown (explicitly deferred behind the microbenchmark); a real
Apache Parquet-page `ColumnCodec` (the `parquet`/arrow-rs crate is not yet a
dependency); re-running this on real multi-VM GCP hardware for real network
latency (currently torn down); the 2-ULP float-serialization quirk.

## Dispatch redesign: bucket config is the gate, not the header (2026-10-02)

Phase 4 originally kept the header's presence as the dispatch trigger (a
hangover from phase 3, before bucket config existed at all) and treated
bucket config as something that only mattered once the header had already
opted a PUT in. User correction: *"We actually don't need that header I
guess if bucket is configured to be already having custom placement. Header
can send false if it explicitly want to default to normal erasure coding if
required."* Content-dependent placement should be a bucket-level decision,
the same as every other bucket setting in this project (versioning, ACL,
retention) — not something a client has to remember to ask for on every PUT.

**New semantics in `cluster_put_object`:**
- **Bucket has no config at all** → straight to the plain path, full stop.
  No header parsing, no packing attempt — the header is irrelevant, even if
  sent. This is also the fast path: one `bucket_config_store.get()` lookup,
  identical cost to before this feature existed.
- **Bucket is configured, header is the literal string `"false"`** → explicit
  per-object escape hatch, forces plain EC for just this one write even
  though the bucket defaults to custom placement. Logged, not an error.
- **Bucket is configured, header carries real `[[offset,len,...],...]`
  data** → unchanged from before: pack, check the bucket's overhead
  threshold, commit or fall back.
- **Bucket is configured, no header at all** → still attempts content-
  dependent placement, treating the whole object as a single unit. This
  needed no special-casing to stay safe: a single unit under RS(k>1,m)
  always costs strictly more than plain EC (seeded into one bin, the other
  k-1 bins pad to its size with nothing to fill them), so the existing
  overhead-threshold check rejects it and falls back on its own — verified
  directly, RS(3,2) with one unit measures **200.000% overhead**
  (`(k+m)/(1+m/k) - 1`), comfortably over any sane default threshold.

**Verified end-to-end** against a live 5-node local cluster, all five cases:
unconfigured bucket with a header present (plain, header ignored); a
configured bucket with a real header (content-dependent, confirmed via
`content_record`); the same bucket with `x-warpd-computable-units: false`
(forced plain); and the same bucket with no header at all against both a
permissive threshold (500%, content-dependent still used) and a realistic
one (2%, correctly falls back with the 200% overhead logged above).
`cargo test` — all 92 tests still pass. `real_workload_driver.py` updated to
explicitly configure its demo bucket (it previously relied on an implicit
per-header default that no longer exists) and reproduces identical overhead
numbers to before this change; `pushdown_benchmark.py` already configured
its bucket explicitly and needed no changes. A follow-up `load_gen` run
(concurrency 16, 4KB payload, no header, no bucket config — the common case)
measured 4553.2 req/s, consistent with the North star's earlier 4589.9–4611.6
req/s band — no regression from reordering which check runs first.

## RS(9,6) means 6 data + 3 parity, not 9 + 6 (found and fixed 2026-10-02)

User's prompt: *"Just check to make sure 9,6. I think they mean six data and
3 parity."* They were right. Confirmed directly against the paper (ASPLOS'25,
Fig. 2 and §2): *"An (n, k) systematic erasure code... k plaintext data
blocks and (n − k) coded parity blocks... A (9, 6) erasure code partitions a
12MB object into two 6MB data stripes, each consisting of 6 data blocks and
3 parity blocks."* Fusion's `(n, k)` is `(total, data)` — the opposite of
this project's own `(k, m)` = `(data, parity)` convention. Reading their "9"
as our `k` is backwards: the correct translation of their default is
`k=6, m=3` (9 total), not `k=9, m=6` (15 total).

This had propagated into three places, all now fixed: `ec_bench.rs`'s
`RS_K`/`RS_M` constants, `main.rs`'s `ec_params_from_env` doc comment, and
the live 15-node cluster test run just before this was caught.

**A bigger finding than just my own mistake**: `hipc_poster/run_all.py` has
the identical bug — `K, M = 9, 6` (line 24), passed straight into
`fac_core.construct_stripes(K, units)` as `K=9`. Every number the Python
simulator has ever reported (the poster draft's 1.16% reproduction check,
its 79.99% Parquet batch-16 figure, everything downstream) was computed
with `k=9`, not Fusion's actual `k=6`. This explains an otherwise-startling
coincidence: re-running the corrected Rust reproduction with the *wrong*
`k=9,m=6` first (before the fix) gave **80.0016%** overhead for Parquet
batch-16 — almost exactly the simulator's own **79.99%** — because it
reproduced the simulator's bug, not Fusion's actual parameter. The poster's
numbers are internally consistent with its own simulator; they're just not
actually RS(9,6) in Fusion's sense. Not fixed here — `hipc_poster/` is
parked, separate scope — but flagged clearly since it affects the poster's
own correctness claims, not just this engine's reproduction of them.

**Real numbers, corrected `k=6,m=3`, 9-node local cluster, poster's own
scale parameters (300k rows / 10 row groups / 16 columns, 20k vectors,
`embed_dim=768`, `nlist=566`):**

| Workload | Granularity | Units | Stripes | Overhead |
|---|---|---|---|---|
| Parquet | column-chunk | 160 | 24 | **0.8014%** |
| Parquet | row-group (batch=16) | 10 | 2 | **20.0361%** |
| Vector (IVF) | finest (gran=1) | 566 | 94 | **1.04%** |
| Vector (IVF) | batch=16 | 36 | 6 | **5.18%** |

All four round-tripped byte-identical. The column-chunk number (0.80%) lands
close to Fusion's own real-10GB-file figure (~1.2%) and the simulator's
reproduction check (1.16%) — same order of magnitude, correct `k` this
time, not a coincidence of a shared bug. The batch-16 number (20.04%) is
genuinely different from the simulator's 79.99%, for the reason above.

One operational note: the very first attempt at this (15-node, wrong
config) hit an intermittent `read quorum not met: 8/9` on one GET that
cleared on retry and did not recur across the entire corrected 9-node run.
Circumstantial evidence it was resource contention from 15 heavy local
processes rather than a real bug, not proof — `packed.rs`'s GET path still
silently swallows `get_shard` errors (`.ok()`, no `warn!`, unlike the plain
path), so there's no log trail to confirm either way. Worth fixing that
logging gap before trusting any future quorum failure's absence.

## Feasibility: does configurability actually serve explainability, and is it safe? (2026-10-02)

Prompted directly: *"Will having this level of configurability help? Towards
explanation? What's the operational performance tradeoffs and is this kind
of system feasible and still ensure liveness, safety and high
availability."*

**Does configurability help explainability, or just add surface area?**
Configurability is what *makes* explainability necessary here, not a
separate feature next to it. With one fixed algorithm, "explainable" would
just mean reading the source once. Because placement is pluggable, the only
way to know what a given bucket actually costs is to measure what it
actually did — which is why `overhead_pct` is computed from the real stripe
result, not a model, and why pushdown's push/no-push decision is a real,
inspectable number (`selectivity × compressibility`), not a heuristic black
box. Pillar 2 exists to carry the weight pillar 3 creates. The honest cost:
the more pluggable the system gets, the less a single global SLA sentence
means — "WarpDrive guarantees X" becomes "WarpDrive guarantees X for this
bucket, given its packer stays under its configured threshold." That's not
a flaw, it's what offering customization actually costs in simplicity.

**Operational performance tradeoffs — three separately-measured things:**
- *The default path costs nothing*, measured directly: 4589.9–4611.6 req/s
  before vs. after all of phases 3/4 existed, same build, same hardware,
  back to back (see the North star's before/after check above). A bucket
  that never configures anything pays zero tax for the machinery existing.
- *Opting in costs very little at write time*: `pack()` is the same
  algorithm Fusion's paper clocks at ~500μs for an 11GB file — negligible
  next to network/disk, which is where PUT latency actually lives (our own
  timing breakdowns already show this). The overhead check runs entirely
  in memory before any shard write, so a rejected pack costs microseconds,
  not a wasted round trip.
- *Not yet measured, honestly*: content-dependent PUT/GET latency against
  the plain path, head to head. That cost is real and inherent, not from
  the configurability layer — multiple independently-placed stripes mean
  more coordination than one whole-object write, by design, traded for the
  63–76% pushdown win on the read side that *is* measured. A real next
  benchmark, not yet run.
- *One deliberate, named tradeoff*: bucket config replicates to every peer
  and requires all acks, not quorum — config disagreement across
  coordinators is worse than config writes being briefly unavailable during
  a partition, a real CAP-style choice, not a free property.

**Feasible while keeping liveness, safety, and HA? Yes today, conditionally
tomorrow.**
- *Safety* holds because the overhead-threshold fallback evaluates before
  committing — a bad pack is discarded in memory, nothing partial is ever
  written, and EC parameters (k, m) stay uniform across all buckets
  regardless of packer choice, so durability doesn't degrade as placement
  gets more customizable, only byte layout does. **Real gap found, and
  corrected mid-discussion (2026-10-02)**: nothing in `packed.rs` validates
  that a packer's output actually covers every unit exactly once. A packer
  that silently drops a unit produces an object with a zeroed gap on GET —
  no error, no log. My first instinct was a bespoke runtime check mirroring
  `packing.rs`'s own property test on `FacPacker`
  (`never_splits_a_unit_and_every_unit_appears_exactly_once`). User's
  correction: that's exactly the wrong shape — hand-written, per-algorithm
  correctness logic is itself bug-prone (the real-time proof: a careless
  offset/length mixup in this same session's own `parquet_real_offsets.py`,
  caught only because a coverage check happened to be written for a
  different reason). The right mechanism is a **generic checksum**, not a
  bespoke partition-coverage check: checksum the original object at PUT
  time, checksum what the packed stripes would reconstruct to, compare,
  before any shard touches the network, same place the overhead-threshold
  check already runs. One mechanism catches dropped units, duplicated
  units, corrupted bytes, and failure modes nobody has thought of yet,
  regardless of whether the bug is in `FacPacker`, a future native packer,
  or eventually a WASM module. This **is** the WASM plan's "simulate for
  correctness" step, not new scope — a placeholder with a concrete
  mechanism now, lightweight and generic rather than a per-algorithm
  property test rewritten for every new packer.
- *Liveness* holds by design: no leader election, no consensus, any node
  serves any request, and the fallback pattern guarantees forward progress
  even when a custom pack is rejected — it never blocks waiting for a
  better one. Gap: once WASM runs arbitrary code, something has to bound
  how long a packer call may take and fall back exactly the way an
  over-threshold pack does today. Same mechanism, not a new one.
- *High availability* holds for node failures (tested: kill a peer, GET
  still succeeds via reconstruction). It does **not** yet hold against
  noisy neighbors — the isolation gap flagged earlier. One bucket's load,
  or once-WASM one bucket's expensive code, can degrade every other bucket
  sharing the same node. Nothing enforces fairness yet.

**Verdict**: feasible and safe right now, specifically because the only
packer in the registry is a trusted, tested, first-party one. Not yet
feasible to safely open that registry to untrusted code without closing two
concrete, scoped gaps first — output-partition validation and per-bucket
resource fairness — both extensions of patterns already built here, not new
architecture. Treat those two as the actual gate on the WASM plan, not a
vague "add security later."

## Phase 5: real queries via DuckDB — Range-GET, and real Parquet bytes (2026-10-02)

User's direction: start the "real recognizable workload engine" proof with
DuckDB (Parquet/SQL), and pick a more object-storage-native vector search
system for IVF later, second. This phase covers what's needed before
DuckDB can query anything at all.

**Found before writing any Rust: the existing workload driver never stored
real Parquet bytes.** `real_workload_driver.py`'s `synthetic_body()` fills
each unit with size-matched filler, explicitly documented as fine for a
storage-overhead measurement (only sizes matter for that), but DuckDB can't
parse filler bytes as Parquet — no real footer, no real magic bytes, no
real column data. Separately, `formats.py`'s `parquet_units`/
`parquet_units_batched` only ever tracked column-chunk *sizes*, never real
file offsets — `fac_core.Unit` has no offset field at all, only `(unit_id,
size)`. Both needed fixing before DuckDB had anything real to read.

**`parquet_real_offsets.py`** (new): computes genuine `(unit_id, offset,
len)` triples from pyarrow's own column-chunk metadata
(`dictionary_page_offset` or `data_page_offset`, spanning
`total_compressed_size`). Checked directly, not assumed: column chunks
written by `pq.write_table` tile the file with zero gaps, in exactly
row-group-major/column-minor order — the same flat order
`parquet_units_batched` already iterates in, confirmed by comparing
append-order against byte-offset-sorted order on a real file. Only two
gaps exist in a whole file: the 4-byte leading `PAR1` magic and the
trailing footer (metadata + length + magic) after the last column chunk.
Both are covered as their own framing units (`_leading_magic`,
`_trailing_footer`), so every byte is covered by exactly one unit — this
module's own `verify_full_coverage` asserts that, and caught a real bug in
my first draft (I'd conflated `column_chunk_byte_range`'s `(start, end)`
return with `(start, length)` when merging batches, producing a
negative-length trailing unit — fixed before anything was PUT).

**`duckdb_demo.py`** (new): PUTs a real Parquet file with real offsets,
verifies byte-identical round-trip **and** that pyarrow can actually
re-open the result (`pq.read_table`) — the real bar, stronger than
byte-equality alone. Verified at both column-chunk and row-group
granularity: byte-identical, pyarrow-readable, correct row count (5000).

**Range-GET on `/cluster/{bucket}/{key}` GET** (new, Rust): didn't exist
before this phase — GET always returned the whole object. Reused the S3
API's own `parse_range_header`/`RangeResult` (widened from `pub(super)` to
`pub(crate)` rather than writing a second parser) and added:
- `packed.rs`: `fetch_and_decode_stripe` extracted as a shared helper from
  the existing whole-object GET, plus a new
  `get_object_content_dependent_range(record, start, end, state)` that
  finds which stripes have *any* unit overlapping `[start, end]`, fetches
  and decodes only those, and copies just the overlapping byte intersection
  of each touched unit into a response buffer sized to the request — not
  the whole object.
- `coordinator.rs`: `cluster_get_object` now takes the request, parses
  Range, and dispatches to the ranged path for content-dependent objects
  or (unoptimized, intentionally) fetches the full object and slices it
  in memory for plain objects — correct either way, but only the
  content-dependent path is selective, which is the entire point of the
  comparison this phase exists to make.

**Verified end-to-end**, real Parquet file, RS(3,2) local cluster: a Range
request for exactly one column chunk's real byte span returned 206,
correct `Content-Range` header, and bytes identical to the real slice of
the original file. A follow-up diagnostic log line
(`range-GET touched N/M stripes`) confirmed the actual selectivity, not
just byte-correctness: **1 of 48 stripes touched** for a single-column-chunk
range, versus all 48 for a whole-object GET. `cargo test`: all 92 tests
still pass, both before and after.

**Not yet done**: DuckDB itself isn't installed in this environment
(`ModuleNotFoundError`, no CLI either) — needs a venv or
`--break-system-packages` given PEP 668. Once installed, the actual
demonstration is: PUT the same real Parquet file into two buckets (one
plain, one FAC-packed at column-chunk granularity), point
`duckdb.sql("SELECT ... FROM read_parquet('http://.../bucket/key')")` at
both via `httpfs`, and show DuckDB's own observed bytes-fetched/latency
differ, caused by nothing but the bucket's placement configuration — an
independent, unmodified tool validating the claim, not our own harness
grading its own homework.

## DuckDB verified live against WarpDrive (2026-10-02)

Installed DuckDB in a venv (`hipc_poster/.venv`, avoids PEP 668 — same
pattern as the `urllib`-over-`requests` decision earlier). Sequencing per
user's direction: verify DuckDB against our own two paths (FAC-packed,
plain) locally first; MinIO joins the comparison later, on GCP, not here —
deferred deliberately, not forgotten.

PUT the same real 2.1MB synthetic-lineitem Parquet file (50k rows) into a
FAC-packed bucket (`facbucket`, column-chunk granularity, 162 units, 52
stripes) and a plain bucket (`plainbucket`, no header), then pointed
`duckdb`'s `httpfs` extension at both via `read_parquet('http://127.0.0.1:
9710/cluster/{bucket}/lineitem.parquet')` — no S3 API involved, DuckDB's
generic HTTP range-reading against our own `/cluster/` endpoint directly.

**Correctness first**: `count(*)`, `avg(extendedprice)`, and a filtered
`GROUP BY` all returned identical, correct results from both buckets.
DuckDB — independent, unmodified, has no idea WarpDrive or FAC exist —
successfully parsed a real Parquet file reconstructed through our
content-dependent placement path.

**Access pattern, confirmed via logs, not assumed**: DuckDB issued 26 real
Range requests against each bucket, identical byte ranges and identical
total bytes (482,600) in both cases — expected and correct, since that's
dictated by the Parquet file's own layout and DuckDB's own column-pruning,
not by WarpDrive. What differs is the *internal* cost of satisfying each of
those 26 requests: every one of them hit `range-GET touched 1/52 stripes`
on the FAC-packed bucket (confirmed in the access log, all 26), while the
plain bucket has no sub-object structure to be selective about, so every
one of those same 26 requests triggers a full `k+m`-shard fetch and full
EC decode of the entire 2.1MB object just to return a small slice.

**Real, reproducible latency difference**, 5 runs per query (first run
dropped as connection/httpfs warmup), same machine, same cluster, same
file:

| Query | FAC-packed (median) | Plain (median) | Speedup |
|---|---|---|---|
| `GROUP BY` with filter (`quantity < 5`) | 3.9ms | 23.7ms | 6.1x |
| `avg(extendedprice)` | 5.0ms | 24.4ms | 4.9x |
| Two-column filter+project | 5.6ms | 40.2ms | 7.2x |
| `count(*)` | 1.6ms | 7.4ms | 4.6x |

Consistent 4.6–7.2x speedup across four differently-shaped queries, from a
completely independent, real SQL engine that is not cooperating with or
aware of WarpDrive in any way — this is the "an unmodified third-party tool
validates the claim" result the DuckDB direction was specifically chosen
to produce, now real, not hypothetical. Local, loopback, single machine —
absolute numbers aren't the point yet; the *relative* effect, caused by
nothing but a bucket config choice, is.

**Next**: the same comparison against MinIO, on GCP, per user's explicit
sequencing — real network latency, a real independent erasure-coded object
store as the third point of comparison, not just WarpDrive's own two paths.

## Lance/IVF: a real new packer, real infrastructure, and a real negative result (2026-10-02/03)

User's framing, correcting an earlier instinct to reuse `FacPacker` for Lance:
*"we have to show that introducing a new [packer] for their workload improves
query latency — reusing FAC is just extra advantage, not what we'd like to
show."* The right experiment is a second, genuinely different `StripePacker`
motivated by how IVF search actually accesses data, proving the registry
pattern itself (bring your own algorithm, bind it to a bucket) — not proving
FAC generalizes. WASM was explicitly descoped for this pass (*"we don't have
to do WASM now... use a different algorithm... look at WASM as well [later]"*)
— `IvfCentroidPacker` is a native Rust `StripePacker`, same registry, no
sandbox yet.

**`Unit::metadata`**: a new opaque `Vec<u8>` field on `packing::Unit`, ignored
by `FacPacker`, read by `IvfCentroidPacker` as a little-endian `u32` spatial
rank. This is the first packer that needs more than a unit's size — the
header format grew an optional 5th base64 element to carry it per-unit.

**`IvfCentroidPacker`**: sorts units by spatial rank, groups every `k`
spatially-adjacent partitions into one stripe (one per bin). Tested in
isolation (`packing.rs`): never splits a unit, no bin exceeds capacity, and
a dedicated test with deliberately scrambled sizes confirms it groups by
rank, not size — the property that actually distinguishes it from `FacPacker`.

**Getting real centroids and real byte offsets out of Lance, not synthetic
ones**: `index_statistics()` gives real per-partition vector counts and
centroids (confirmed: `ds.create_index(..., ivf_centroids=...)` accepts
pre-trained centroids directly, so the centroids used for spatial ranking
are *exactly* the ones that determined the real index's layout, not a
separately-trained, possibly-mismatched set). `LanceFileReader(aux_file)
.metadata()` gives the real physical buffer position/size for the
`__pq_code` column. One declared, not silently assumed, approximation:
an 80-byte constant (`buffer.size - sum(counts)*8`) is treated as a fixed
header before flat per-partition data — checked for arithmetic consistency
only, not verified against Lance's own (unpublished) micro-layout spec.
`_rowid`'s own encoding is non-flat and not sliced per-partition; folded
into a framing unit instead, same treatment as Parquet's leading magic
bytes. `lance_real_offsets.py`'s own `verify_full_coverage` confirmed zero
gaps/overlaps on a real built index (285 units, 992,004 bytes).

**New Rust infrastructure, all required just to make Lance's own
unmodified S3 client connect at all** (not optional polish — Lance's
`object_store::aws` client needs real bucket/list semantics; their own docs
use `"endpoint": "http://minio:9000"` as the canonical example, nothing like
DuckDB's arbitrary-URL `httpfs`):
- `LocationStore`/`ContentLocationStore` grew a `list(bucket, prefix)`
  method — the one genuinely new primitive (GET/PUT/HEAD already existed).
- `s3_shim.rs`: minimal ListObjectsV2 (merges both stores, no pagination)
  and HEAD, under a new `/cluster/s3/` prefix; GET/PUT reuse
  `cluster_get_object`/`cluster_put_object` directly. Deliberately not a
  full S3 implementation — no SigV4 verification, scoped to exactly what
  `object_store::aws` needs. **Real bug found and fixed in the first
  smoke test**: HEAD returned `Content-Length: 0` for an 11-byte object —
  actix recomputes Content-Length from the actual (empty) `.finish()` body,
  silently discarding a manually-inserted header. Fixed by reusing
  `HeadBody` (already built for the S3 API's own HEAD handler, for the
  same reason), widened from `pub(super)` to `pub(crate)`.
- **Real bug found uploading the first actual dataset**: gRPC's default
  4MB message limit rejected a ~20MB shard from a real Lance data fragment
  (`"message length too large: found 20491849 bytes, the limit is:
  4194304 bytes"`) — never hit before because every previous test object was
  smaller. Fixed: `WARPDRIVE_GRPC_MAX_MESSAGE_SIZE`, default 256MB, set on
  both the gRPC server and client.
- **Real bug found in `upload_dataset`**: uploaded keys didn't carry the
  `data.lance/` prefix Lance's reader expects (it opens
  `s3://bucket/data.lance`, so it looks up `data.lance/_versions/...`, not
  bare `_versions/...`). Fixed.
- **Real bug found in `get_object_content_dependent_range`**: the loop
  over needed stripes was sequential (`for stripe in needed { ...await...
  }`), not concurrent — fine when a request touches 1 stripe (the common
  case), serializing N round-trips end to end when it touches many (an
  outlier real query touched 55 of 95). Fixed with `try_join_all` over all
  needed stripes at once.

**After every one of those fixes, Lance's own unmodified S3 client opened
both buckets correctly** (`count_rows=20000` on both) and ran real
`nearest={"column": "vector", "q": ..., "nprobes": N}` ANN queries
successfully, returning correct, matching results from both. The
mechanism is real and proven end to end — not a connectivity claim, a
working one.

**The actual performance result is a clean negative, reported honestly,
not papered over.** `IvfCentroidPacker`-packed queries were consistently
*slower* than plain, not faster — roughly 580–620ms (plain) vs 1650–1820ms
(ivf_centroid) across nprobe 4/16/40, 9 queries each, full RS(3,2) local
cluster. Root-caused, not just observed: the per-request stripe-touch
distribution has a real right tail (median 1 of 95 stripes touched — good
— but mean 4.1, max 55). A direct, controlled single-Range-GET comparison
bypassing Lance entirely showed only a modest raw per-call cost gap (3.24ms
vs 1.53ms) — nowhere near enough to explain the full-query gap — so the
real cost is concurrency contention on the rare high-fan-out calls: 55
stripes needing `(k+m)=5` peer RPCs each is 275 simultaneous calls
contending over an 8-connection gRPC pool and 8 actix workers, even with
the sequential-fetch bug fixed.

**Why, structurally, not just empirically**: a greedy nearest-neighbor
*chain* linearizes 768-dimensional centroids into one 1-D ordering, which
only guarantees that *consecutive* points in that specific chain are
mutually close. An arbitrary query's `nprobe`-nearest centroids are a
*ball* in 768-dimensional space — nothing about a 1-D linearization
guarantees an arbitrary ball maps to a contiguous interval of that chain.
This is a real, principled gap in this specific heuristic, not a bug in
the surrounding infrastructure, and not a refutation of the underlying
thesis (Parquet's column-chunk case has no such gap, because a column
chunk's "who needs this together" relationship — the same query always
wants the same column — is exact, not probabilistic).

**Not done, and worth naming plainly**: a packer that actually clusters
centroids (e.g. k-means into `nlist/k`-sized groups, each group becoming
one stripe) rather than linearizing them would far more reliably keep an
arbitrary query's probed set within one or two stripes, and is the
principled next attempt if this experiment continues. Not yet built.
WASM execution for a packer (any packer) remains fully deferred, per this
session's explicit descoping, independent of this result.

## Lance/IVF, continued: real k-means, FAC-pack-per-cluster, and the real bottleneck (2026-10-03)

**A real confound found and fixed first**: a bucket configured for content-
dependent placement doesn't require a header on every object — a missing
header still *attempts* packing with a synthesized single whole-object
unit (by design, so a client that forgets the header still gets a correct
answer). For every file in the `lanceivf` bucket *other* than the one aux
file actually being tested — the ~60MB data fragment included — this meant
silently packing each as one oversized, zero-padded stripe (measured: a
single unit under RS(3,2) is a real 200% overhead, comfortably inside this
demo's generous 500% bucket threshold) instead of falling back to plain's
even k-way split. Found by timing, not inspection: a single data-fragment
fetch logged at ~150ms with ~61MB-sized shards, repeated on every result
row materialized. **This was the actual cause of the earlier 3x slowdown,
not the packing algorithm.** Fixed in `lance_demo.py`'s `upload_dataset`:
every non-aux file now explicitly sends `x-warpd-computable-units: false`,
forcing plain EC regardless of bucket config. After this fix alone, full
end-to-end query times closed from ~580ms vs ~1700ms to ~580ms vs ~600ms —
roughly even, not a win, but no longer a confound either.

**Random test vectors were also a confound, found by direct measurement,
not suspicion**: checked 768-dim i.i.d. Gaussian pairwise distances
directly — they concentrate tightly around the theoretical
`sqrt(2*dim)≈39.2` regardless of which pair is measured (observed: 38.2,
40.4, 40.8 across three arbitrary pairs). There is no "nearby" structure in
such data for *any* spatial packer to exploit, by construction — this is
why the greedy-chain version showed no improvement no matter how it was
tuned. Fixed: `lance_demo.py` now generates vectors as draws from 40
random "topic" blobs (`make_blob_structured_vectors`), the same shape real
embeddings actually have (similar items genuinely closer together), which
is the entire reason IVF indexing is a sound technique on real data in the
first place.

**User's correction, acted on**: *"even FAC has multiple stripes and each
stripe[sic] unit has multiple computable units within a data block."*
`IvfCentroidPacker` was wrongly restricted to one partition per bin. Fixed:
extracted FAC's own greedy bin-packing loop into a standalone `fac_pack(k,
units)` helper (`FacPacker` is now a thin wrapper over it), and rewrote
`IvfCentroidPacker` to group units by *exact* k-means cluster id (not
sorted-order proximity — the earlier chain-ranked version only guaranteed
chain-*adjacent* units were close, not that a whole cluster's members
landed together once a cluster had more than `k` members) and run
`fac_pack` *within* each cluster. `lance_real_offsets.py`'s
`kmeans_cluster_order` (flattened rank) replaced with
`kmeans_cluster_assignments` (real cluster id per partition) to match.
Four packing.rs tests cover this: never-splits/no-overflow (now with
repeated cluster ids, not synthetic rank values), exact-cluster-id
grouping (not size-based), and — new — multiple same-cluster units
correctly sharing bins instead of needing one stripe per `k` partitions
regardless of true cluster size.

**Result of the full chain of fixes**: mean stripes touched per query
dropped from ~3.5 to ~3.1 (real, measured, modest), but total stripe count
rose from 95 to 118 (uneven real cluster sizes waste some bin capacity — a
genuine trade-off, not a bug) and the tail persists (up to 51 of 118
touched) — plausibly inherent to the query distribution itself (topic
assignment is uniform random across 40 topics; a query near several
topics' boundary legitimately needs partitions from several different real
clusters) rather than a packing defect. Full end-to-end query time:
unchanged at ~600ms vs ~605-640ms, slightly slower, not faster.

**The real finding, isolating the aux file from end-to-end noise**: a
direct aux-file-only measurement (`columns=[]`, `with_row_id=True`, no row
materialization) showed single-digit-to-tens-of-ms latency for *both*
buckets, noisy, no clear winner either direction. **The auxiliary.idx
partition-locality problem, however well solved, cannot move the needle on
full query latency, because it was never where the latency lives.** The
~580-600ms end-to-end cost is almost entirely the "base-table take" —
reconstructing the (still-plain, ~60MB) data fragment file to materialize
10 result rows' `id`/`vector` values, paid 2-3 times per query. The
principled next move, if this continues, is applying content-dependent
placement to the *data fragment* by row (or row-batch) — the same move
already proven out for Parquet row groups — not further refinement of the
aux-file packer, which is now a solved, correctly-behaving, but largely
irrelevant-to-latency piece of this picture.

## Lance/IVF: the real win — row-batch packing the data fragment (2026-10-03)

Did exactly the move named above. **`lance_data_fragment_real_units`**
(new, `lance_real_offsets.py`): real per-row-batch computable units for a
Lance data fragment file — the one `take` actually reads to materialize
result rows, not the auxiliary index. Checked directly before building
anything on top: the `vector` column (`FixedSizeList<float32>[768]`) is a
single flat buffer starting at byte 0, size *exactly*
`num_rows * 768 * 4` — zero header, zero padding, confirmed by exact
arithmetic match. The `id` column uses a different, non-flat encoding
(sequential integers compress well: 34,752 bytes measured for what a flat
int64 layout would need 160,000 for) — same treatment as `_rowid` and
Parquet's magic bytes before it: folded into one opaque trailing framing
unit together with the file's footer, not sliced per-row. 1,000 row-batch
units (batch size 20 rows ≈ 60KB each) plus one trailing framing unit,
verified full coverage against a real built file (1001 units, 61,475,306
bytes, exact).

**Deliberately no spatial/cluster metadata on these units** — unlike IVF
partitions, which rows a `take` needs are essentially uniform-random
scattered row ids (whichever 10 happen to rank highest after PQ-distance
scoring), with no "nearby rows get queried together" relationship to
encode. Tagging these units with no cluster id metadata makes
`IvfCentroidPacker`'s own fallback path (units with no metadata all land
in one group, `cluster_id = u32::MAX`, then get real `fac_pack` applied
within it) degenerate into exactly `FacPacker`'s size-based bin-packing —
the right tool for "many small same-size units, no locality to exploit."
Reuses the bucket's existing fixed packer policy with zero new Rust code;
doesn't need a third packer.

**`upload_dataset` generalized** from one hardcoded aux-file special case
to a `real_units_by_rel_path: dict` — any number of files in a dataset can
now get real computable-unit treatment; everything else still gets the
explicit `x-warpd-computable-units: false` escape hatch.

**Result: a real, decisive, verified win.** Same cluster, same real
queries, same correctness bar as every other measurement in this
document:

| nprobe | plain (median) | row-packed `lanceivf` (median) | speedup |
|---|---|---|---|
| 4  | 579.7ms | 15.5ms | 37.4x |
| 16 | 587.4ms | 20.6ms | 28.5x |
| 40 | 579.9ms | 21.4ms | 27.1x |

**Correctness checked before trusting the number**, same discipline as
every prior claim here: 5 independent query trials, comparing both
returned row ids *and* actual vector values (not just counts) between the
plain and packed buckets — all 5 trials, exact match on both ids and
byte-for-byte vector data (`np.allclose`). The speedup is real, not a
silently-wrong-but-fast result.

**Why this (and not the aux file) was always the real lever**: the
auxiliary.idx partition-selection problem is genuinely a locality problem
(which centroids are near which), and no packing strategy around it was
ever going to be more than a few-millisecond effect, because the file
itself is under 1MB. The data fragment is 60MB+, plain, and reconstructed
whole on every `take` — that's a structural, not incidental, 60MB-vs-60KB
difference, and content-dependent placement is exactly the mechanism built
this entire session to address exactly that gap. The lesson generalizes:
before optimizing a placement strategy, measure *which file* actually
dominates latency — the right fix can be a complete reuse of existing
machinery (no new packer, no new Rust) applied to the right target,
rather than a cleverer algorithm applied to the wrong one.

## Replacing synthetic data with a real, published benchmark (2026-10-03)

User's prompt: *"Can we use actual lance workload or any benchmarks that's
real which asks real questions."* Everything up to this point used
synthetic test vectors (random noise, then topic-blobs). Switched to
SIFT1M-small, from the texmex corpus — the same dataset LanceDB's own
published benchmarks (GIST-1M and SIFT-1M) report recall/latency numbers
against, so results here are comparable to Lance's own claims, not just
internally consistent.

**Source**: `ftp.irisa.fr/local/texmex/corpus/siftsmall.tar.gz` is blocked
from this network (403); used the byte-identical mirror published by
`TileDB-Inc/TileDB-Vector-Search`'s GitHub releases instead. Real 10,000
base vectors (128-dim), 100 real queries, real ground truth (100,100) —
**verified independently before trusting it**: ground truth for query 0
checked against a from-scratch brute-force exact nearest-neighbor search
over the real base vectors, exact match, not assumed correct because it
came from a named source.

**`sift_benchmark.py`** (new): loads the real `.fvecs`/`.ivecs` files,
builds a real Lance IVF_PQ index (100 partitions, 16 sub-vectors) with
real centroids, reuses every piece of infrastructure already built —
`lance_ivf_real_units`, `lance_data_fragment_real_units`,
`upload_dataset`, the S3 shim — unchanged. One real bug found adapting to
a different dataset shape, fixed immediately: `lance_data_fragment_real_units`
assumed the `vector` column's buffer started at byte 0 (true for the
earlier 768-dim/20k-row dataset, false here — column write order on disk
isn't guaranteed to match schema declaration order). Fixed by looking up
the column by name via the real schema (`md.schema.get_field_index
("vector")`) and treating whatever precedes/follows its buffer as
leading/trailing framing, instead of assuming position 0.

**Results, real queries, real ground truth, asking the real question a
vector-search benchmark is supposed to ask (not just "is it fast" — "is it
still correct"):**

| nprobe | plain (median) | packed (median) | speedup | recall@10 (both, identical) |
|---|---|---|---|---|
| 5  | 6.53ms | 1.30ms | 5.0x | 0.691 |
| 10 | 6.41ms | 1.07ms | 6.0x | 0.732 |
| 20 | 6.24ms | 1.21ms | 5.2x | 0.738 |

Smaller magnitude than the earlier 20k-row/768-dim result (27-37x) — this
dataset's data fragment is 5.1MB, not 61MB, so the plain path's whole-file
reconstruction cost is proportionally smaller to begin with — but the
mechanism holds on genuinely real, externally-verified data: **recall@10
is bit-for-bit identical between the plain and packed buckets at every
nprobe level**, confirming placement strategy and search correctness are
fully independent, exactly the invariant that should hold (a pure storage
layout change must never change which neighbors are found, only how fast
they're fetched). This is the same per-row-batch data-fragment packing
already proven on synthetic data, now validated against an authoritative,
independently-checked, externally-recognizable benchmark.

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
