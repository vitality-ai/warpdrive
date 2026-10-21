#!/usr/bin/env python3
# Author: Tejus Chandrashekar
"""
Consistency checker for gcp_orchestrator.py's recorded history. Three
outcome categories, not two -- a real distinction this run's own first
pass needed before trusting the result (see docs/CHAOS-TESTING.md):

  - CORRUPTION: a read returned bytes matching no write attempt at all
    (confirmed or not). A real safety violation.
  - STALE: a read returned a value strictly older than the last
    *confirmed* write that completed before the read began. A real
    consistency violation.
  - UNRESOLVED-AMBIGUOUS-WRITE: a read returned a value that matches
    only an unconfirmed (client-side ok=False) write attempt. Under a
    real network partition, a write can time out on the client while
    still completing server-side -- the client's logged completion time
    for that case is not a trustworthy ordering anchor (it reflects when
    the client gave up, not when the server actually finished, if it did
    at all). This is a genuine ambiguity a real linearizability solver
    (Knossos/Elle-style) would resolve formally; this lighter-weight
    checker reports it rather than guessing in either direction.

Usage:
    python3 gcp_checker.py [history.json] [nemesis_log.json]
"""
import json
import sys

HISTORY = sys.argv[1] if len(sys.argv) > 1 else "/tmp/gcp_jepsen_history.json"
NEMESIS = sys.argv[2] if len(sys.argv) > 2 else "/tmp/gcp_nemesis_log.json"
KEYS = [f"k{i}" for i in range(5)]
BUCKET = "jepsen-gcp-bucket"

history = json.load(open(HISTORY))
nemesis = json.load(open(NEMESIS))

for h in history:
    if isinstance(h.get("value"), str):
        h["value"] = h["value"].encode()

writes = [h for h in history if h["type"] == "write"]
reads = [h for h in history if h["type"] == "read"]
print(f"=== history: {len(history)} client ops, {len(nemesis)} nemesis events ===")
print(f"writes={len(writes)} ({sum(1 for w in writes if w['ok'])} ok) reads={len(reads)} ({sum(1 for r in reads if r['ok'])} ok)")
for ev in nemesis:
    print(f"  nemesis: {ev}")

violations = []
# A write the client logged as ok=False can still have landed server-side
# (the request can succeed after the client gives up waiting -- an
# "ambiguous outcome" under a real network partition/timeout, not data
# loss). So *any* attempted write's value is a legitimate thing a read
# may see; only a value matching no attempted write at all is corruption.
all_written_values = {w["value"] for w in writes}
all_confirmed_values = {w["value"] for w in writes if w["ok"]}

for key in KEYS:
    # Ordering ("last write completed-before this read") is only
    # trustworthy anchored on CONFIRMED (ok=True) writes -- an ok=False
    # write's logged t_complete is when the CLIENT gave up, which is not
    # a reliable bound on when the server actually finished (if it did
    # at all). Using an unconfirmed write's timestamp to assert ordering
    # produces exactly the unresolvable false positives this run first
    # hit; see docs/CHAOS-TESTING.md.
    key_writes = sorted([w for w in writes if w["key"] == key and w["ok"]], key=lambda w: w["t_complete"])
    key_reads = [r for r in reads if r["key"] == key and r["ok"]]
    for r in key_reads:
        val = r["value"]
        if val is None:
            continue
        if val not in all_written_values and val != f"init-{key}".encode():
            violations.append(("CORRUPTION", key, r, "value was never written by this run"))
            continue
        prior = [w for w in key_writes if w["t_complete"] <= r["t_invoke"]]
        if not prior:
            continue
        last_completed_before = prior[-1]
        if val == last_completed_before["value"]:
            continue
        overlapping_or_newer = [
            w for w in key_writes
            if w["t_invoke"] <= r["t_complete"] and w["t_complete"] >= last_completed_before["t_complete"] and w["value"] == val
        ]
        if overlapping_or_newer:
            continue
        if val not in all_confirmed_values:
            # The value matches only an *unconfirmed* write attempt -- the
            # client never got a success response for it, so there's no
            # trustworthy timestamp to judge "stale" against. This is a
            # genuine, unresolvable ambiguity (the same kind a real
            # linearizability solver like Knossos/Elle exists to settle
            # formally), not a demonstrated violation.
            violations.append(("UNRESOLVED-AMBIGUOUS-WRITE", key, r,
                                "saw a value whose only matching write was never confirmed by the client "
                                "(request likely timed out during real chaos, but may have landed server-side)"))
            continue
        violations.append(("STALE", key, r,
                            f"saw a value from before the last write completed-before this read "
                            f"(last_completed_before t_complete={last_completed_before['t_complete']:.3f})"))

print(f"\n=== {len(violations)} consistency violations found ===")
for kind, key, r, detail in violations:
    print(f"  [{kind}] key={key} thread={r['thread']} t_invoke={r['t_invoke']:.3f} t_complete={r['t_complete']:.3f}: {detail}")
if not violations:
    print("\nNO CONSISTENCY VIOLATIONS across the real multi-machine GCP run.")
