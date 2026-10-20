# Cluster chaos/fault-tolerance tests

Fault-injection tests against a real local 5-node RS(3,2) WarpDrive
cluster -- each one starts its own cluster, injects real faults
(process kills, on-disk byte corruption), and checks that the
cluster's quorum/erasure-coding guarantees actually hold. See
[docs/CHAOS-TESTING.md](../../../docs/CHAOS-TESTING.md) for
methodology, what these runs have found so far, and what they
deliberately do not cover yet.

## Files

- `scenario_test.py` -- fast, specific checks of named claims (quorum
  tolerates up to `m` peer failures and no more, a corrupted shard is
  never served as correct, a peer killed mid-write doesn't corrupt the
  write). Seconds to run, entirely local.
- `jepsen_style_test.py` -- a concurrent random workload plus an
  independent nemesis thread (random kill/restart/corrupt, on its own
  schedule, unaware of the workload) running at the same time, for 90
  seconds by default, with a post-hoc consistency checker over the full
  recorded operation history. Entirely local; this is the one that
  actually explores interleavings rather than testing hypotheses picked
  in advance.
- `gcp_orchestrator.py` / `gcp_workload.py` / `gcp_checker.py` -- the
  same methodology run against a real 5-node GCP cluster instead of
  local processes, so faults can include genuine `iptables` network
  partitions between separate machines, not just process kill. Needs a
  pre-existing GCP cluster (see docs/CHAOS-TESTING.md's "Real
  network-partition testing on GCP" for setup and the real result it
  found). `gcp_orchestrator.py` deploys and launches `gcp_workload.py`
  on node-0 and drives nemesis actions over SSH; `gcp_checker.py`
  analyzes the result afterward and is the only one of the three meant
  to be run standalone (the other two are driven by `gcp_orchestrator.py`
  or deployed by it).

## Usage

```bash
(cd ../../.. && cd server && cargo build --release --bin warp_drive)
python3 scenario_test.py
python3 jepsen_style_test.py                      # 90s run
JEPSEN_DURATION_SEC=300 python3 jepsen_style_test.py   # longer run

# against a real GCP cluster (must already exist -- see docs/CHAOS-TESTING.md):
python3 gcp_orchestrator.py 300
python3 gcp_checker.py
```

`scenario_test.py` and `jepsen_style_test.py` manage their own local
cluster lifecycle (start 5 nodes fresh, kill/restart nodes as part of
the test, stop everything at the end) -- no cluster needs to be running
beforehand, and none is left running afterward. They use
`/tmp/wd-chaos` and `/tmp/wd-jepsen` as disposable scratch storage, and
`jepsen_style_test.py` also writes the full operation history to
`/tmp/jepsen_history.json` for manual post-hoc inspection.

`gcp_orchestrator.py` restarts/heals every node at the end and leaves
the 5 GCP nodes running, and writes its history/nemesis log to
`/tmp/gcp_jepsen_history.json` / `/tmp/gcp_nemesis_log.json`.
