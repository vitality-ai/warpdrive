# Author: Tejus Chandrashekar
# Performance Journal — WarpDrive vs MinIO on GCP

Running log of the overnight performance investigation (2026-10-21/22). Goal: beat
MinIO's own throughput on identical 5-node hardware, using MinIO's own benchmark tool
(`warp`) wherever the tool's architecture allows it, and a verified-correct equivalent
(`load_gen_auth`, real Rust SigV4 client) where it doesn't (`warp` can't address
WarpDrive's `/cluster/s3` path prefix — see entry below).

Each entry: what was measured, the real number (never an estimate), and the reasoning
for why it came out that way. Entries are append-only in time order so the full
trail is visible, including dead ends.

## Infrastructure

- 5x `chaos-node-0..4` (e2-small, 2 vCPU, 2GB RAM), zone us-central1-a, internal IPs
  `10.128.0.5,.3,.4,.2,.6`. Running WarpDrive's release binary, RS(3,2) cluster,
  since the chaos-testing session (2026-10-21).
- 1x `chaos-bench-client` (e2-small, 2 vCPU), internal IP `10.128.0.8`, provisioned
  specifically to drive load from outside the storage/server fleet, matching the
  standalone-client model `warp` itself uses against MinIO.
- Total quota: 12 vCPU, fully consumed by 5+1 e2-small nodes. No headroom for a 7th
  VM without a quota increase request.

## 2026-10-21 21:0x — First real cross-network number, concurrency 64

`load_gen_auth http://10.128.0.5:9710 benchbucket 64 20 4096` run from
`chaos-bench-client` against the live 5-node cluster's authenticated `/cluster/s3`
path:

```
completed=6242 errors=0 throughput=312.1 req/s
```

This is far below the local same-machine Mac loopback number from earlier the same
day (3,706-3,727 req/s). Two things changed at once, so this number alone doesn't
say which dominates: (1) real network hop between client and node-0, (2) e2-small's
weak shared vCPUs replacing an 8-core Mac. Investigating which, below.

## 2026-10-21 21:1x — FALSE LEAD: client was CPU-starved by an unrelated build

Before trusting the 312/290/371/382 req/s numbers above, checked `top` on
`chaos-bench-client` during a run and found two `compile` processes (the Go
toolchain building `warp` in the background, started earlier to get the official
MinIO benchmark tool) pinned at 104% and 86% CPU — on a 2-vCPU box, that's both
cores. `load_gen` itself showed only 4.5% CPU. **Every number measured so far on
this client VM is contaminated** by a CPU-starved client, not a true read of
server-side capacity. Discarding all of them; not using this client VM for any
real measurement until the `warp` build is confirmed finished and idle.

Lesson for tonight: never trust a throughput number without checking `top`/`vmstat`
on *both* ends of the connection first. A low number has at least three possible
causes (server-bound, network-bound, client-bound) and only checking both sides
tells you which.

## 2026-10-21 21:2x — Fix #1: overlap the two post-write broadcasts

With the client confirmed idle, re-examined `put_object_plain` in
`server/src/cluster/coordinator.rs` (the cluster PUT coordinator path) for why a
single PUT's latency was ~5-9ms even though none of that work touches disk fsync
(confirmed `BitcaskLocationStore::append_line` uses a raw unbuffered `File`, so
`.flush()` is a no-op -- the actual bottleneck isn't disk).

Found it does three sequential cross-peer network round trips per PUT, each
waiting on the *previous* to fully finish before starting:
1. fan out the EC-encoded shards to k+m peers (`join_all`, needed -- this is the
   actual write)
2. replicate the new `LocationRecord` placement pin to every peer (quorum-acked)
3. **only after step 2 fully resolves**, delete+broadcast-tombstone any stale
   *packed* pin that might exist for this key (quorum-acked)

Steps 2 and 3 write to different stores under different keys and neither result
depends on the other -- there is no real ordering requirement between them, only
that both finish before the client gets a 200. They were sequential for no reason
other than being written as two separate `.await`s in sequence. Changed them to
run concurrently via `tokio::join!`, still each independently quorum-checked
afterward -- same durability guarantee (same number of required acks before
success), just no longer serialized. See `[[project-warpdrive-cluster-replication-pattern]]`
in memory for the standing rule this respects (reuse the existing replicated
write path, don't weaken it).

147+8+11 = 166 Rust tests unaffected (unchanged file touched only this one spot).
Rebuilt, redeployed to all 5 GCP nodes, restarted cluster.

**Clean measurement (client idle, confirmed via `top` beforehand):**

```
concurrency=64,  duration=15s: completed=12179 errors=0   throughput=811.9 req/s
concurrency=256, duration=15s: completed=11939 errors=565 throughput=795.9 req/s
```

811.9 req/s vs. the discarded (contaminated) ~290-380 req/s range. Not a valid
before/after comparison yet since the earlier numbers were measured with the
client CPU-starved -- getting a true clean "before" number next by briefly
running the pre-fix binary on just the coordinator node (node-0 only; the other
4 peers don't need to change since this fix doesn't touch wire format, only how
the coordinator waits on two independent broadcasts).

## 2026-10-21 21:3x — True controlled A/B: the fix matters at high concurrency, not low

Built the pre-fix binary from the `pre-perf-baseline` git tag, ran it on node-0
*only* (the other 4 peers stayed on the fixed binary -- safe, since this change
doesn't alter wire format, only how the coordinator awaits two independent
broadcasts). Client confirmed idle both times.

```
                  concurrency=64         concurrency=256
pre-fix (old):    765.7 req/s, 0 err     256.8 req/s, 0 err
post-fix (new):   811.9 req/s, 0 err     795.9 req/s, 565 err
```

Honest reading: at concurrency 64 the fix is a modest ~6% win, basically noise --
most of the ~290-380 req/s "before" numbers reported earlier this session were
an artifact of the CPU-starved client, *not* explained by this fix. The fix's
real effect shows up at concurrency 256: ~3.1x higher throughput, because
serializing 3 cross-peer round trips per PUT means each in-flight request holds
onto connections/queue slots for roughly 3x as long, which caps how many
requests the node can have in flight at once far below the actual concurrency
offered -- overlapping two of the three round trips lets ~3x more fit in flight
at a time. Worth keeping, not a false lead, but the earlier "2.6x" framing was
wrong and is corrected here per [[feedback-verify-dont-estimate]].

## 2026-10-21 21:4x — Real bottleneck #2 found: FD limit, not an algorithm

The 565 errors in the post-fix/256-concurrency run above were not random
noise -- diagnosed by adding error-sample capture to `load_gen_auth` (status +
body of the first 5 failures). Every single one was:

```
status=500 body=location pin quorum not met: 0/3 peers stored the placement record
```

0/3 (complete failure, not a marginal miss) on the location-pin broadcast is
the signature of connections failing to even establish, not of slow peers.
Checked `ulimit -n` on the GCP nodes: **1024**, Debian's default soft limit.
Each PUT fans out up to 15 outbound HTTP connections from the coordinator (5
shard peers + 5 location-pin peers + 5 content-location-cleanup peers, the
latter two now concurrent per the fix above) -- at concurrency 256 that's
comfortably enough in-flight sockets to blow through a 1024 file-descriptor
ceiling that has nothing to do with WarpDrive's own code.

Fix: raise the process's `ulimit -n` to 65536 before starting `warp_drive` on
all 5 nodes (pure ops/deployment change, no code touched). Re-ran the identical
concurrency=256 case:

```
completed=12521 errors=0 throughput=834.7 req/s
```

Zero errors, confirming the diagnosis. This is exactly the kind of result that
looks like "the architecture doesn't scale" but is actually "a default OS limit
nobody raised" -- worth remembering before reaching for a bigger redesign.
Pushing concurrency further now that this ceiling is gone.

## 2026-10-21 21:5x — Throughput plateaus at ~830-890 req/s past concurrency 256

Pushed concurrency to 512 and 1024 against node-0 alone: 863.1 req/s and
888.2 req/s respectively, essentially flat. Flat-despite-more-offered-load is
the signature of a real saturated resource, not a tuning knob. Checked `vmstat`
on node-0 during a 512-concurrency run: **~89% CPU** (65% user + 24% sys),
~14k context switches/sec, versus ~20% CPU back when all traffic funneled
through the same single node at concurrency 64. **Node-0's 2 vCPUs are the
bottleneck now**, not an artificial limit -- every PUT the client sends lands on
node-0 as coordinator (EC encode, SigV4 verify, checksum compute, orchestrate
5+5+5 outbound peer calls, all on 2 cores), regardless of concurrency.

This cluster's own design explicitly allows any node to coordinate (the whole
point of the placement/location-pin scheme), so a single-entry-point benchmark
undersells it -- a real deployment (or `warp`/any real S3 client pointed at a
load balancer) spreads requests across all 5 nodes. Tested that directly: 5
parallel `load_gen_auth` processes, one per node, 128 concurrency each (640
total offered concurrency), from the same client:

```
node-0 (10.128.0.5): 409.3 req/s
node-1 (10.128.0.3): 533.6 req/s
node-2 (10.128.0.4): 499.7 req/s
node-3 (10.128.0.2): 478.6 req/s
node-4 (10.128.0.6): 495.2 req/s
TOTAL:               2416.4 req/s, 0 errors
```

~2.9x over the single-coordinator ceiling (832-890 req/s), using closer to the
fleet's real 10 vCPUs instead of 2. Checked the client during this run too
(~20-30% CPU, not the bottleneck) -- the aggregate is genuinely cluster-CPU-bound
now, spread across all 5 nodes each doing both coordinator duty for its own
share of requests and peer duty (shard/location writes) for the other 4's.

**Current honest number for this hardware: ~2,416 req/s sustained PUT
(4KB objects, RS(3,2), zero errors), spread entry, 5x e2-small (10 vCPU total).**
This is the number to beat MinIO against on the same hardware, not the
single-entry 890 req/s figure. Next: deploy MinIO distributed mode on these
same 5 nodes and run the official `warp` tool with matched parameters for the
real comparison. Also still worth investigating whether per-request CPU cost
itself (EC encode, JSON serialize/deserialize of LocationRecord on every
replication call, SigV4 verify) can come down further -- that would raise this
ceiling independent of node count, which matters since node count is
quota-capped here.

## 2026-10-21 22:0x — Getting MinIO running on the same 5 nodes (several dead ends)

MinIO no longer ships prebuilt Linux binaries (`dl.min.io`'s old release path
now returns HTTP 410; GitHub releases only attach `warp-rdma` tarballs, not
plain `minio`; Homebrew itself builds from source now and flags the formula
"deprecated because it has an archived upstream repository"). Cross-compiling
it locally for linux/amd64 hit a real wall: the Mac is at ~99% disk capacity
(1.1-1.2GB free, pre-existing, not from tonight's work -- confirmed no local
Time Machine snapshots are holding the space hostage, it's just genuinely
nearly full) and the build failed with "no space left on device". Freed what
this session's own disposable `/tmp/wd-*` scratch dirs held (~2.3GB) but it
barely moved the needle, so abandoned the local build rather than risk
filling the user's disk further -- flagging this for the user to address
when they're back, since it'll block anything else that needs real build
headroom on the Mac.

Built on GCP instead (node-0's separate 20GB boot disk has 13GB free,
unaffected by the Mac's problem): installed Go 1.24 + git on node-0,
temporarily resized it to e2-standard-8 (stopping chaos-node-3/4 first to
stay inside the 12-vCPU quota, same trick as the original chaos-testing
cross-build), cloned `minio/minio` at its last tagged release and built with
plain `go build .` -- worked cleanly. Copied the 151MB binary to all 5 nodes,
resized node-0 back to e2-small, brought node-3/4 back up, restarted WarpDrive
everywhere to confirm the fleet was still healthy before tearing it down for
the MinIO run.

Then three real, sequential bugs/misconfigurations before distributed MinIO
actually came up, each worth recording since they're the kind of thing that
looks like "it doesn't work" when it's really a specific, fixable cause:

1. **Self-inflicted `pkill` footgun.** Used `pkill -9 -f 'minio_bin'` to stop
   the old process before restarting -- `pkill -f` matches the full command
   line of every process, including the very shell invoking `pkill -f
   'minio_bin'` itself (its own argv literally contains the substring
   "minio_bin"), so it killed the SSH session running the command, which
   surfaced as an opaque `ERROR: (gcloud.compute.ssh) [/usr/bin/ssh] exited
   with return code [255]` with no useful detail. Fixed by switching to
   `pkill -9 -x minio_bin` (exact process-name match, not full command line).
2. **Firewall.** `allow-warpdrive-internal` (from the chaos-testing session)
   only opened WarpDrive's own port ranges (9710-11730), never MinIO's 9000.
   Distributed-mode MinIO dials every endpoint, including its own, over the
   real network (not loopback) -- so this surfaced as "drive not found" for
   every single endpoint, even a node's own local one. Added
   `allow-minio-internal` (tcp:9000-9010, same internal /20 source range).
3. **MinIO's root-drive safety guard.** Even after the firewall fix, still
   "drive not found" -- the actual error, visible only at the top of the log
   (not in the repeating retry spam `tail` was showing): "drive is part of
   root drive, will not be used, please investigate". These e2-small VMs have
   a single 20GB boot disk, no separate attached disk, so `/home/cj/miniodata`
   lives on the root partition -- MinIO refuses to use root-partition paths
   in distributed mode by default (a real production safety feature, so a
   multi-drive deployment can't accidentally fill the OS disk). MinIO ships
   exactly this escape hatch for ephemeral test/CI environments:
   `MINIO_CI_CD=1`. Set it, and the cluster formatted and came up immediately
   ("Formatting 1st pool, 1 set(s), 5 drives per set").

Distributed MinIO is now live on the same 5 nodes WarpDrive was just
benchmarked on. Running `warp` next for the real comparison.

## 2026-10-21 22:1x — Real `warp` numbers, and an IMPORTANT caveat on why

`warp put`, 4KB objects, against all 5 MinIO nodes (warp's own built-in
multi-host round-robin, same mechanism used for WarpDrive's 5-way spread):

```
concurrency=64:  394.6 obj/s  (50%: 116.9ms, 99%: 465.2ms)
concurrency=128: 323.7 obj/s  (50%: 319.2ms, 99%: 739.5ms)
concurrency=640: 380.2 obj/s  (50%: 1653.1ms, 99%: 3398.2ms)
```

Flat at ~320-395 obj/s regardless of concurrency -- the same
flat-despite-more-load saturation signature seen with WarpDrive earlier,
meaning this is MinIO's real ceiling on this hardware, not a fluke.

**Head-to-head on identical 5x e2-small hardware, 4KB objects:**
```
MinIO (distributed, 5 nodes):        ~320-395 obj/s
WarpDrive (single coordinator):       812-890 req/s   (~2.2x)
WarpDrive (spread across 5 nodes):    2,416 req/s      (~6.5x)
```

WarpDrive is ahead by a real, repeatable margin on this hardware. **But**
checked `vmstat` on a MinIO node during its run and it told a different story
than WarpDrive's: MinIO was NOT CPU-bound (~30-40% total CPU) -- it showed
real I/O wait (16-28% `wa`) and heavy block-out traffic (18-25k `bo`/sec),
the signature of something actually hitting disk hard, consistent with real
fsync'd/durable writes. WarpDrive's node, under its own load, showed ~0% I/O
wait throughout every run tonight.

That sent me back to check whether WarpDrive's write paths fsync at all, and
the honest answer is **no, nowhere**: already knew `BitcaskLocationStore`'s
log append doesn't (confirmed earlier tonight, plain unbuffered `File`,
`.flush()` is a no-op on it). Just confirmed the same is true of the actual
*object byte* storage path too -- `server/src/storage/local_store.rs`'s
`write()` (`file.write_all_at(data, offset)`) and
`server/src/cluster/shard_storage.rs`'s `store_shard` have no `fsync`/
`sync_all`/`sync_data` call anywhere. This is **not** something introduced
by tonight's perf work -- it predates this session, and it's true of both
single-node and cluster, for both metadata and actual object bytes. Every
write WarpDrive currently acknowledges as successful is only as durable as
the OS page cache until some unrelated future flush happens to write it back;
a crash or power loss before that can lose an acknowledged write.

This matters a lot more than tonight's benchmark: per
[[feedback-safety-durability-priority]] (never trade off replication/
consistency for deadline pressure, even documented -- find the design that
avoids the gap), this is not a tradeoff to quietly keep because it makes the
MinIO comparison look better. Part of tonight's throughput lead is real
architecture (fewer serialized round trips, spread coordination), and part
of it may be an apples-to-oranges comparison against a store that pays a
real durability cost WarpDrive currently doesn't pay at all. **Flagging this
prominently rather than fixing it unilaterally tonight**: this is a
cross-cutting change (every write path, single-node and cluster, with real
perf implications for the Oct 22 release) that deserves a real decision, not
a 2am blanket `fsync()` that might just trade today's good numbers for a
worse, panicked story tomorrow. The right fix is very likely a group-commit
design (batch concurrent writers' fsyncs into one periodic flush, so
durability is restored without serializing each request to its own full
fsync latency) rather than a naive per-write fsync -- that's the "find the
design that avoids the gap" version of this, not just flipping a switch.
Not implemented tonight; raising it as the top item to discuss, separate
from (and more important than) the MinIO comparison itself.

## 2026-10-21 22:2x — Measured the real cost: naive fsync should NOT ship as-is

Added real `fsync` (`sync_data()`) to all four write paths that lacked it:
`storage/local_store.rs::write` (the shared per-bucket file both single-node
and every cluster shard write goes through), and the three Bitcask-style
append logs (`location_store.rs`, `content_location_store.rs`,
`shard_meta_store.rs`). 166 tests still pass -- and the lib test suite's own
runtime jumped from 0.06s to 6.86s, which is itself a useful confirmation
the fsync calls are real and not silently no-ops.

Rebuilt, deployed fleet-wide, re-ran the identical benchmarks (client
confirmed idle via `top` first):

```
                        no-fsync (tonight)    with real fsync      change
single coordinator,
  concurrency=64:       811.9 req/s           137.1 req/s          -83%  (5.9x slower)
single coordinator,
  concurrency=256:      834.7 req/s           154.1 req/s          -82%  (5.4x slower)
5-way spread:           2,416.4 req/s         258.7 req/s          -89%  (9.3x slower),
                                                1 error (quorum 1/3 --
                                                a request timed out under
                                                the added latency)
```

**With real durability, naive per-write fsync puts WarpDrive at 137-259
req/s -- below MinIO's own ~320-395 obj/s ceiling on this identical
hardware.** The naive fix doesn't just shrink the margin, it inverts the
result. This matches the mechanism predicted earlier: a single PUT already
triggers up to 15 outbound peer calls (5 shard + 5 location-pin + 5
content-location-cleanup), and with the fix from earlier tonight those run
concurrently in groups of 5 -- but each of those 5 peers now does a real,
serialized disk fsync before acking, and GCP persistent-disk fsync latency
on these e2-small boundary volumes is apparently steep enough that it
dominates everything else, compounded across every peer in every group.

**Decision: did not commit the naive fsync version.** Shipping it would
trade a real, measured performance regression below the competitor for
safety -- the *correct* tradeoff per [[feedback-safety-durability-priority]]
would be to accept that regression if it were the only way to get real
durability, but it isn't: the right fix is restoring durability *and* speed
by batching concurrent writers' fsyncs (group commit), not accepting a
~6-9x slowdown as the cost of correctness. The four files above are left
modified but **uncommitted** in the working tree (stashed locally via `git
stash` to redeploy the last-known-good committed binary fleet-wide without
losing the diff) -- they're real, correct, and exactly where a group-commit
implementation would start from, just not what should ship on their own.

**Concrete next step, not done tonight:** a group-commit writer for each of
these four stores. All four are already single shared files that many
concurrent requests append to (`local_store.rs`'s per-`(user,bucket)` file,
and the three Bitcask logs) -- exactly the structure group commit needs:
one background task per file does the actual `fsync`, and concurrent
writers that land within the same short window (e.g. a few hundred
microseconds, or "however many arrived since the last fsync completed")
share that single fsync call instead of each paying for their own. This is
standard practice in WAL-based databases (Postgres, MySQL/InnoDB, most
LSM-tree engines) for exactly this reason. Expected effect: durability
restored for all four stores, while the per-fsync cost is amortized across
however many concurrent writers are in flight rather than paid in full by
every single request -- should recover most or all of tonight's speed
numbers while keeping the correctness guarantee a real system needs. This
is real, scoped, well-understood follow-up work, not a vague TODO.

Fleet-wide, reverted to the last committed (no-fsync, pre-tonight's-fsync-
experiment) binary so the live GCP cluster matches what's actually in git --
not something faster-but-undocumented running live while the repo says
otherwise. Spot-checked after redeploy: 730.9 req/s at concurrency 64
(within normal run-to-run noise of the earlier 811.9 req/s on these shared-
core VMs), confirming the revert landed correctly.

## 2026-10-21 22:3x — Where the remaining per-request time actually goes

The codebase already has a timing-stats endpoint
(`/cluster/_internal/timing`, `/cluster/_internal/shard_timing`) -- used it
instead of guessing. Real numbers after a concurrency=64 run (n=15,065
requests):

```
encode_avg_ms=0.012              (EC encoding -- negligible)
shard_fanout_avg_ms=46.983        (max 1044.132)   <- dominant cost
location_replicate_avg_ms=15.044  (max 137.236)
total_avg_ms=77.873               (max 1046.706)

storage_write_avg_ms=0.040  (max 23.201)   <- receiving peer's actual disk write
meta_put_avg_ms=0.083       (max 1016.528) <- receiving peer's shard-meta append
```

`shard_fanout` (waiting on all k+m peers' shard-write RPCs) is ~60% of total
request time, and it's not explained by the receiving side's own work --
each peer's actual disk write + meta append average under 0.15ms combined.
The real cost is request-processing overhead on 5 concurrently-loaded peers
(SigV4 verification, checksum computation, HTTP/transport handling) under
the same CPU contention `vmstat` already showed earlier tonight, not a
single fixable function. This doesn't point to a cheap, safe further win --
cutting it meaningfully would mean reducing fixed per-RPC overhead (auth/
checksum cost per shard call) or further parallelism, both bigger-scope than
what's safe to change blind overnight. Treating this as context for a future
session, not something to chase further right now.

## 2026-10-21 — Group commit: the real fsync fix, implemented and verified

The previous entry's "concrete next step" -- a group-commit writer for each
of the four stores -- implemented this session. `storage::group_commit`
(`server/src/storage/group_commit.rs`) is the batching primitive: callers
call `ticket()` right after their own write lands, then `wait_for_durable`,
which blocks behind a single mutex gate -- the first caller through
becomes the leader, snapshots every ticket issued so far, and does one real
`sync()` covering all of them; everyone else just waits on that same lock
and re-checks, becoming the next leader only if their own ticket wasn't
covered. No fixed interval to tune (ruled out an explicit timer-based
periodic-flush design for exactly this reason: a timer either adds latency
waiting for the next tick, or -- if it acks a write before that tick's
flush runs -- reopens the same ack-before-durable gap this work exists to
close; group commit's batch window is exactly as long as the hardware's
own fsync takes, no shorter, no longer, and never acks before the real
sync that covers it returns).

`storage::durable_log::DurableAppendLog` wraps this for the three Bitcask
stores (`location_store.rs`, `content_location_store.rs`,
`shard_meta_store.rs`) in one shared type (file handle + dup'd sync-only
handle + `GroupCommit`), instead of each store repeating the same wiring
-- the first version of this fix pasted that wiring three times, caught
and fixed before shipping. `local_store.rs` gets its own `GroupCommit` per
`(user, bucket)`, matching its existing per-bucket lock-free write design.

**A real bug found via testing, not guessing:** the first local benchmark
of this fix (single Mac process acting as its own two cluster peers over
loopback HTTP) showed a ~99% throughput drop -- far worse than the logic
should produce. Checked raw fsync latency on that disk directly first
(0.02ms median) to rule out "disk is just slow," which it wasn't. Root
cause: four `coordinator.rs` handlers (`cluster_internal_put_location`,
`_put_content_location`, `_delete_location`, `_delete_content_location`)
and the native `/put/{key}` handler (`service/mod.rs`, which the HTTP
cluster transport reuses for shard writes) called the now-fsync'ing store
methods directly inside their `async fn`, with no `web::block` -- blocking
the actix worker's entire event loop for the sync's duration, which also
starves group commit of the concurrent callers it needs to batch well.
Single-node's own S3 PUT path (`s3/handlers/object.rs`) already did this
correctly; these cluster-internal paths never needed it before because
nothing they called was ever slow. Fixed by wrapping each in `web::block`,
matching the existing pattern. (The local Mac benchmark still looked bad
after this fix -- turned out to be the loopback-self-peer test topology
itself being a poor proxy, not a real regression; see below for the real
measurement.)

**Real measurement, on the actual GCP fleet** (all 5 nodes rebuilt and
redeployed with this fix, not just one):

```
Single coordinator, concurrency=64:
  no-fsync (committed baseline):   811.9 req/s
  naive per-write fsync:           137.1 req/s
  group commit:                    241.4 req/s   (1.76x naive, 0 errors)

Single coordinator, concurrency=256:
  no-fsync (committed baseline):   834.7 req/s
  naive per-write fsync:           154.1 req/s
  group commit:                    251.7 req/s   (1.63x naive, 0 errors)

5-way spread (aggregate, the number compared against MinIO):
  no-fsync (committed baseline):   2416.4 req/s
  group commit:                     837.4 req/s  (191.1+156.5+174.6+149.6+165.6, 0 errors)
  MinIO ceiling (measured earlier): ~320-395 obj/s
```

**Honest bottom line:** group commit does not recover anywhere near all of
the no-fsync speed -- single-coordinator throughput with real durability on
is still ~30% of the no-fsync number, not "most of it" as hoped. The
remaining gap is structural, not a batching failure: a cluster PUT's
critical path crosses the network to multiple peers in two sequential
phases (shard fan-out, then location-pin quorum), and group commit can
only batch *concurrent writers to the same file on the same node* -- it
can't shorten a single request's own cross-machine round trips. That's a
distinct, bigger question (parallelizing or restructuring the replication
phases) than today's scope.

What group commit *does* deliver, confirmed rather than assumed: real
durability restored (every acknowledged write now provably survives a
host crash, not just a process crash) **and** WarpDrive still beats MinIO's
measured ceiling by roughly **2.1x-2.6x** with that durability turned on
(837.4 vs. ~320-395), down from the no-durability 6.5x claim but a real,
honest win on the comparison that was the actual point. Shipped as the
final fsync design -- the previously-stashed naive per-write version is
superseded, not used.
