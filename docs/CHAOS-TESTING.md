# Chaos / fault-tolerance testing of the distributed engine

## Why

This project's core claim is that the cluster tolerates peer failures
without losing or corrupting data (RS(k,m) erasure coding, quorum
writes/reads). That claim had only ever been checked by hand (a
cross-node multipart test, `fanout_bench`) -- nothing exercised it under
real, concurrent, randomized failure. This session's own checksum work
found a real gap of exactly this shape (a corrupted shard could be
served to a client uncaught) before it was fixed, which is the direct
motivation for testing this systematically rather than trusting the
design on paper.

## What this is, and what it deliberately is not

The scripts in `server/tests/chaos/` are not the real Jepsen
(Clojure, needs a control node to SSH into Docker/VM-orchestrated test
nodes) and not the real Netflix Chaos Monkey (needs Spinnaker managing
a cloud-provider instance group to pick instances to terminate --
neither tool has a way to attach to raw local processes on one Mac).
What they do implement is the actual *methodology* both of those tools
are built around, directly against this project's real local cluster:

- A **nemesis** that runs independently and continuously, on its own
  random schedule, with no knowledge of what client traffic is doing --
  not a scripted "do X, then check Y" scenario.
- A concurrent **client workload** generating real contention and real
  races, recording a full **operation history** (invocation time,
  completion time, type, key, value, success/failure).
- A **post-hoc checker** that walks the recorded history after the run
  and flags actual consistency violations, rather than only checking
  each operation's own immediate response.

This is real value, not a simulation: every fault (a process actually
dying, a byte actually flipped on disk) is real, and the cluster under
test is the real binary. What it does *not* yet cover is **real network
partitions** -- everything here is single-machine, so "a peer is down"
only ever means "the process is dead," never "the process is alive but
some nodes can't reach it while others can," asymmetric partitions,
added latency, or packet loss. Historically, partition-handling bugs
are where the nastiest real distributed-systems bugs live (this is
Jepsen's own most common finding across the systems it has tested), and
this local setup cannot produce that class of fault. That requires real
separate machines with a real network between them -- see "Next step"
below.

## How to run

```bash
cd server && cargo build --release --bin warp_drive
cd tests/chaos
python3 scenario_test.py          # ~15s, fast named checks
python3 jepsen_style_test.py       # 90s, concurrent nemesis + workload + checker
```

Both scripts start and stop their own 5-node cluster; nothing needs to
be running beforehand. See `server/tests/chaos/README.md` for details.

## Results so far (2026-10-21, as of commit `22a0759`)

### `scenario_test.py` -- 13/13 passed

- RS(3,2) write/read succeeded with 0, 1, or 2 of 4 non-entry peers
  killed (tolerates up to `m=2`, as designed).
- Write **correctly and cleanly failed** (never silently succeeded with
  an incomplete quorum) with 3 or 4 peers down.
- A deliberately corrupted on-disk shard was never served as correct
  data: every post-corruption read was either reconstructed exactly or
  rejected with a `500` (the read-side checksum verification added
  earlier this session actually doing its job under real corruption,
  not just in the isolated test that motivated it).
- Killing 1 of 5 peers mid-write (timed to land while the write is
  actually in flight, not before it starts) never corrupted the write;
  the object read back byte-identical afterward.

### `jepsen_style_test.py` -- 0 consistency violations across 9,215 operations

90-second run, 5 client threads (one exclusive writer per key, reading
any key), one independent nemesis thread. Nemesis fired 36 times
(kills, restarts, on-disk corruptions), with stretches of up to 4 of 5
nodes down simultaneously. Result: 5,485 writes (5,376 ok), 3,694 reads
(3,602 ok), **0 corruption violations, 0 stale/lost-update violations**.

**A real false-positive, caught and fixed before trusting the result:**
the first version of this test let all client threads write to the same
small set of keys, and the checker flagged 24 "stale read" violations.
Investigating them found the real cause was the checker, not the
cluster: with multiple independently-clocked connections racing to
write the *same* key within ~10-40ms of each other, client-observed
completion order doesn't reliably reflect the order the server actually
applied them in -- a real limitation of checking consistency via wall
clocks across separate connections, not a data-loss bug. Fixed by giving
each thread exclusive ownership of one key for writes (eliminating the
ambiguity entirely, since a single thread's writes are strictly ordered
by its own sequential code) and keeping reads random across all keys.
Rerunning with that fix produced the 0-violation result above. This is
itself worth keeping in mind for any future version of this checker: at
low enough latency, same-key multi-writer contention needs either a
stricter check (e.g. requiring a large enough mutation gap) or a real
linearizability solver (Knossos/Elle-style), not a simple wall-clock
heuristic.

## Known gaps

- **Static crash audit** (same session, before the chaos runs): this
  codebase uses `std::sync::Mutex`/`RwLock` with `.lock().unwrap()`
  pervasively (66+ call sites in `metadata/sqlite_store.rs` alone,
  similar in the cluster's in-memory stores), with no poison-recovery
  pattern anywhere. A panic in any handler while holding one of these
  locks doesn't crash the process (Tokio isolates per-request panics
  fine, and `Cargo.toml` doesn't set `panic = "abort"`), but does poison
  the lock permanently -- every subsequent request touching it then
  panics too, forever, until restart. No live trigger for this was
  found (a ~35-case live adversarial-input probe across malformed
  ranges, keys, multipart part numbers, ACL/tagging fields, and more
  found zero crashes), so this is a structural fragility, not a proven
  live bug: one future bug anywhere in a locked section becomes a
  permanent outage instead of one failed request.
- A real, non-crash correctness bug was found and fixed in the same
  audit: `UploadPart`/`UploadPartCopy` never validated `partNumber`
  against S3's real 1-10000 range (confirmed not exploitable as a crash
  -- `part_number` is never used as a raw array index anywhere).
- The corruption test flips bytes in on-disk shard files directly; it
  does not yet corrupt shards *in flight* (mid-transfer over gRPC/HTTP/
  TCP) or exercise the content-dependent (FAC/packed) read path the
  same way the plain path's checksum fix was verified against earlier.
- No clock-skew injection, no disk-full/ENOSPC injection, no slow-disk
  injection.

## Next step: real network partitions

Standing up the literal Netflix Chaos Monkey (needs Spinnaker) or the
literal Jepsen framework (Clojure, needs Docker- or VM-orchestrated
nodes reached over SSH) is not realistic to get right in a day even
with infrastructure available -- Spinnaker alone is normally a multi-day
install, and a trustworthy from-scratch Jepsen nemesis/client/checker
for a new system is routinely a multi-day-to-multi-week effort even for
experienced Jepsen authors. What real infrastructure (e.g. GCP VMs)
*does* unlock that a single Mac cannot: real `iptables`-based network
partitions between genuinely separate machines over SSH, extending the
same methodology in `jepsen_style_test.py` with a new nemesis action
(`partition` / `heal`) in place of local process kill. That is the
concretely useful next step if cloud resources become available, not a
rebuild of the existing harness.
