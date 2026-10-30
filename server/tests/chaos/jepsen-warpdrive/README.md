# Jepsen / Elle DST harness for WarpDrive

Deterministic-simulation-style fault-injection testing of the distributed
engine, using the **real Elle checker** from the Jepsen ecosystem (not a
hand-rolled one). It drives a real local RS(3,2) cluster and the real S3 API
while a *seeded* nemesis injects faults, then hands the operation history to
Elle's `rw-register` cycle-detection analysis.

This is **"Jepsen as a library."** Full `jepsen.core` expects nodes it reaches
over SSH (its Docker/VM topology); this box has neither, so we manage local
processes and faults ourselves (`cluster.sh`) and use the genuine Jepsen
pieces that matter for correctness — Elle, and a Jepsen-format history.

## What it injects

| Fault | Mechanism | Why it matters |
|---|---|---|
| Process kill / restart | `SIGKILL` + relaunch | node loss |
| **Network partition** | `iptables` drop on the node's port | node is **alive but unreachable** — process keeps its locks/timers/state; what a kill can't simulate, and the gap the project's own CHAOS-TESTING doc flagged as untested |
| On-disk corruption | flip a byte in a stored shard | exercises read-side checksum verification |
| **Power loss** | `SIGKILL` the whole cluster, recover on the same data | directly tests the group-commit durability guarantee (a 200 must mean durable) |

Partitions here isolate a node at the port level (single host, no per-node IPs
without iproute2), so they are per-node isolation, not a clean two-sided
split-brain. That is still strictly more than a process kill. Documented, not
hidden.

## Why the results are trustworthy

`lein run selftest` is a **negative control**: it feeds Elle a history with a
planted aborted-read (G1a) and asserts Elle reports `valid? false`. A green
`test` run only means something because this proves the checker has teeth.

## Running

Prereqs: a release binary built WITH the DST failpoints
(`cd server && cargo build --release --bin warp_drive --features failpoints` --
without `--features failpoints` the `fail_point!` macros compile to no-ops and no
failpoint can ever fire), JDK 21, and [Leiningen](https://leiningen.org).
`iptables` is used for partitions (run as root, or the script uses `sudo`).

Failpoints are armed per-run via the `FAILPOINTS` env var, inherited by the
nodes (e.g. `FAILPOINTS="coordinator::before_pin=return" ./cluster.sh start`):
  - `coordinator::before_pin=return` -- crash the coordinator after shard quorum
    but before the location pin is replicated (the ack-vs-pin window).
  - `local_store::before_fsync=return` -- crash a node after the write() syscall
    but before the durability wait (the power-loss window).
  - `local_store::skip_fsync=return` -- ack a write WITHOUT waiting for
    durability; a regression control to confirm the durability check bites.
All three are validated to fire (see commit message); `=20%return` fires
probabilistically for the nemesis.

```bash
cd server/tests/chaos/jepsen-warpdrive

lein run selftest                 # negative control: Elle must catch a planted G1a
lein run validate                 # SigV4 S3 client smoke test against a running node:9710
lein run durability --m 200       # power-loss: write 200 acked keys, SIGKILL all, recover, assert survival
lein run test --seed 42 --duration-sec 60 --concurrency 8   # seeded Jepsen+Elle consistency run
```

Every fault schedule is a pure function of `--seed`, so a failing run replays
exactly by re-running with the same seed.

## Files

- `project.clj` — deps: `jepsen`, `elle`, `clj-http`, `cheshire`
- `src/jepsen_warpdrive/s3.clj` — minimal S3 client with hand-rolled AWS SigV4
  (the real authenticated `/cluster/s3` path, same API the boto3 harness uses)
- `src/jepsen_warpdrive/core.clj` — workload, seeded nemesis, Elle check,
  durability test, selftest
- `cluster.sh` — start/stop/kill/restart/isolate/heal/corrupt/crashall/recover
