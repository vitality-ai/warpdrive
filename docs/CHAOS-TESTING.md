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
test is the real binary. `scenario_test.py` and `jepsen_style_test.py`
run entirely on one machine, so "a peer is down" only ever means "the
process is dead" there -- not "the process is alive but some nodes
can't reach it while others can," which is a different and historically
nastier class of bug (Jepsen's own most common finding across the
systems it has tested). `gcp_orchestrator.py` extends the same
methodology onto 5 real separate GCP VMs specifically to close that gap
-- see "Real network-partition testing on GCP" below.

## How to run

```bash
cd server && cargo build --release --bin warp_drive
cd tests/chaos
python3 scenario_test.py          # ~15s, fast named checks
python3 jepsen_style_test.py       # 90s, concurrent nemesis + workload + checker
```

Both scripts start and stop their own local 5-node cluster; nothing
needs to be running beforehand. `gcp_orchestrator.py` is different -- it
drives a real 5-node GCP cluster that must already exist (see "Real
network-partition testing on GCP" below for setup). See
`server/tests/chaos/README.md` for details on all four scripts.

## Results so far (2026-10-21, as of commit `22a0759`)

### `scenario_test.py` -- 13/13 passed

- RS(3,2) write/read succeeded with 0, 1, or 2 of 4 non-entry peers
  killed, matching the designed `m=2` tolerance.
- With 3 or 4 peers down, writes failed cleanly with an explicit error
  -- the correct behavior once quorum can't be met.
- Every post-corruption read of a deliberately corrupted on-disk shard
  came back either exactly reconstructed or rejected with a `500` --
  the read-side checksum verification added earlier this session doing
  its job under real corruption, confirming the earlier isolated test
  for real.
- Killing 1 of 5 peers mid-write, timed to land while the write was
  actually in flight, still produced a byte-identical object on
  read-back.

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

## Real network-partition testing on GCP

Standing up the literal Netflix Chaos Monkey (needs Spinnaker) or the
literal Jepsen framework (Clojure, needs Docker- or VM-orchestrated
nodes reached over SSH) was not realistic to get right in a day --
Spinnaker alone is normally a multi-day install, and a trustworthy
from-scratch Jepsen nemesis/client/checker for a new system is routinely
a multi-day-to-multi-week effort even for experienced Jepsen authors.
What real GCP VMs unlock that a single Mac cannot: real `iptables`-based
network partitions between genuinely separate machines over SSH.
`gcp_orchestrator.py` extends the exact same methodology as
`jepsen_style_test.py` (independent nemesis, concurrent workload, full
history, post-hoc checker) onto 5 real VMs, with a `partition`/`heal`
nemesis action alongside kill/restart/corrupt.

**Setup** (5 `e2-small` VMs, `chaos-node-0..4`, one zone): the release
binary was cross-built once on a temporarily-resized larger VM (building
this project's dependency tree on a 2-vCPU box was impractically slow)
and copied to all 5 nodes, which then ran at normal `e2-small` size for
the actual test. `gcp_workload.py` is deployed to and runs on node-0,
issuing requests against its own `localhost:9710`; `gcp_orchestrator.py`
runs locally and drives every nemesis action over `gcloud compute ssh`
(kill/restart a peer's process, add/remove `iptables` DROP rules between
two real peers, flip a byte in a peer's on-disk shard file).

### Result (2026-10-21): 0 confirmed consistency violations across 20,379 real operations

300-second run, 5 GCP VMs, 23 nemesis events (kills, restarts, on-disk
corruptions, and genuine `iptables` partitions -- confirmed for real
before the full run: a partitioned node could not reach its target while
an unpartitioned third node remained fully reachable, and reachability
returned cleanly after healing). 12,273 writes (12,256 client-confirmed),
8,106 reads (8,099 client-confirmed). **0 corruption, 0 stale/lost-update
violations.**

**Two flags the checker first raised, investigated and resolved as
expected behavior, not bugs:** two reads of key `k2` returned a value
("T2-S879-...") that didn't match any *client-confirmed* write. Tracing
it back: that exact write had been issued, took 10.47 seconds under a
concurrent real network partition, and the client gave up and logged it
as failed (`ok: False`) -- but it had in fact landed server-side (RS(3,2)
quorum didn't require the partitioned peer), and a later read correctly
observed it. That's the system doing the *correct* thing under an
ambiguous-outcome timeout -- a write the client couldn't confirm wasn't
lost -- not a data-loss bug. The checker's bug was treating only
client-confirmed writes as "things that happened": fixed by adding a
third outcome category, `UNRESOLVED-AMBIGUOUS-WRITE` (a value matching
only an unconfirmed write attempt, correctly left unresolved rather than
asserted either way, since an unconfirmed write's client-logged
completion time isn't a trustworthy ordering anchor -- exactly the kind
of ambiguity a real linearizability solver like Knossos/Elle exists to
settle formally). After the fix, the same two reads land in that
informational category, and genuine `CORRUPTION`/`STALE` are both zero.
See `gcp_checker.py`'s own docstring for the three-category design.

### Reproducing

```bash
gcloud auth login
gcloud config set project <PROJECT_ID>
gcloud services enable compute.googleapis.com
gcloud compute firewall-rules create allow-ssh-chaos --network=default --allow=tcp:22 --target-tags=chaos-node
gcloud compute firewall-rules create allow-warpdrive-internal --network=default \
  --allow=tcp:9710-9730,tcp:10710-10730,tcp:11710-11730 --source-ranges=10.128.0.0/20 --target-tags=chaos-node
gcloud compute instances create chaos-node-0 chaos-node-1 chaos-node-2 chaos-node-3 chaos-node-4 \
  --zone=us-central1-a --machine-type=e2-small --image-family=debian-12 --image-project=debian-cloud \
  --tags=chaos-node --boot-disk-size=20GB
# build the release binary (on a temporarily bigger VM if e2-small is too slow to compile on),
# copy it + server_log.yaml to ~/warpdrive/server/ on every node, then:
cd server/tests/chaos
python3 gcp_orchestrator.py 300
python3 gcp_checker.py
```

Project-specific names/IPs/zone are hardcoded at the top of
`gcp_orchestrator.py` -- adjust them for a different GCP project.
