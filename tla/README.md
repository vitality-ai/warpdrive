# TLA+ Specifications for WarpDrive

Formal models of WarpDrive's storage engine, checked with TLC. Two
modules, layered: single-node first, then the distributed cluster built
on top of it.

## Files

| File | Models |
|---|---|
| `WarpDrive_SingleNode.tla` / `.cfg` | One process, one local store. PUT, GET, DELETE, UPDATE. |
| `WarpDrive_Distributed.tla` / `.cfg` | The cluster layer in front of it: peers, placement, quorum, retention. |

The distributed module does not replace the single-node one. It extends
it. `Init` starts the distributed model with exactly one peer, so
single-node is the `|peers| = 1` case of the distributed spec, not a
separate model. At the bottom of `WarpDrive_Distributed.tla`,
`SingleNodeView` instantiates `WarpDrive_SingleNode` against a
projection of the distributed state, so `WarpDrive_SingleNode`'s own
`Consistent` invariant can be checked directly against the distributed
model, not just claimed in a comment.

## What the distributed model adds

Each piece maps to a real file in `server/src/cluster/`:

- **Membership** (`membership.rs`): peers can join. Nothing ever removes
  a peer or rebalances existing data.
- **Placement** (`placement.rs`): a deterministic function of the
  current peer set, resolved and pinned into a per-key record at PUT
  time.
- **PUT vs. GET/DELETE** (`coordinator.rs`): PUT resolves placement
  fresh and pins it. GET and DELETE only ever read the pin. A peer
  joining later never changes where an existing object's shards are
  looked up.
- **Quorum**: write succeeds once `k` peers ack (`k+1` if `m = k`). Read
  succeeds once `k` of the pinned peers are up. Erasure-coded
  reconstruction is modeled as "enough pinned peers are up," not literal
  Reed-Solomon math.
- **Retention**: `retentionMode`, `retainUntil`, and `legalHold` live on
  the same record as the placement pin. One lookup answers both "where
  are the shards" and "is this object locked," so the two can never
  drift apart.

The full mapping is in the comment block at the top of
`WarpDrive_Distributed.tla`.

## Invariants checked

**Single-node:** `DataConsistency`, `NoDuplicateKeys`, `UserIsolation`,
`Consistent`.

**Distributed**, on top of the above:
- `QuorumWasMet` — a key only exists if its write reached quorum.
- `PlacementPinSubsetOfKnownPeers` — a pin only ever names real peers.
- `ComplianceIsUndeletable` — a COMPLIANCE-locked, unexpired object can't
  be deleted by any action in the spec. This is the property that rules
  out retention-check and delete-enforcement drifting apart across two
  code paths (the failure class behind seaweedfs#8350/#11333).
- `JoinNeverMutatesRecords` — a peer joining never touches an existing
  key's pin or retention fields.

## Verified

TLC 2.19, run 2026-10-11. Both configs pass clean:
- `WarpDrive_SingleNode.tla`: 81 states, <1s.
- `WarpDrive_Distributed.tla`: 41,526 states, depth 12, ~2s.

Sanity-checked that the checker isn't passing vacuously: temporarily
removed `GovernanceOverrideDelete`'s GOVERNANCE-only guard, reran, and
TLC immediately found a counterexample (lock a key COMPLIANCE, delete it
four steps later). Restored the guard and reconfirmed the clean run
before committing anything.

## Running

```bash
java -jar tla2tools.jar WarpDrive_SingleNode.tla
java -jar tla2tools.jar WarpDrive_Distributed.tla
```

Get `tla2tools.jar` from https://github.com/tlaplus/tlaplus/releases.

## Open for later

- A full refinement proof relating every distributed action to a
  single-node one, step by step. `SingleNodeView` today is a lighter
  structural check, not that proof.
- Content-dependent placement, once a second `PlacementPolicy` lands in
  code.
- Consensus, network partitions, multi-node replication beyond the
  join-only membership model above.
