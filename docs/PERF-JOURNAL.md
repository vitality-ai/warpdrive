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
