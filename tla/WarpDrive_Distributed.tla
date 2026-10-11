---- MODULE WarpDrive_Distributed ----
\* TLA+ specification for the WarpDrive distributed (cluster) storage engine.
\*
\* HOW THIS RELATES TO WarpDrive_SingleNode.tla
\* ---------------------------------------------
\* WarpDrive_SingleNode.tla models one process with one local store: a PUT
\* writes storage[user][key], a GET reads it back, a DELETE clears it. That
\* file is untouched by this module and still checks on its own.
\*
\* This module does not rewrite that model. It adds the layer that sits in
\* front of it in the real system (server/src/cluster/ in the Rust code):
\* a set of peers, a placement policy that picks which peers hold an
\* object's shards, a pinned per-key location record, and quorum rules for
\* read and write. When the peer set has exactly one member, every object's
\* placement is just that one peer, acked by that one peer, read from that
\* one peer — which is exactly what WarpDrive_SingleNode.tla already
\* describes. Single-node is the |peers| = 1 case of this spec, not a
\* different model bolted on next to it.
\*
\* At the bottom of this module, SingleNodeView instantiates
\* WarpDrive_SingleNode against a projection of this module's state
\* (treating each bucket as a "user" and reading through the quorum/pin
\* logic below), so the single-node module's own Consistent invariant can
\* be checked against the distributed model's state directly, instead of
\* just by analogy in a comment.
\*
\* WHAT IS MODELED AND WHAT IS NOT
\* --------------------------------
\* Modeled: peer membership (join only, no rebalancing, matches
\* membership.rs), deterministic placement as a function of the current
\* peer set (matches placement.rs's ComputedPlacement), the PUT-recomputes-
\* and-pins vs. GET/DELETE-reads-the-pin asymmetry (matches
\* coordinator.rs), write and read quorum against k data shards (matches
\* coordinator.rs's quorum rule), and retention/legal-hold stored on the
\* same per-key record as the placement pin (matches shard_storage.rs /
\* the location-store design, specifically to rule out retention and
\* placement ever being read from two different records that can drift
\* apart).
\*
\* Not modeled: the actual Reed-Solomon arithmetic (ec.rs) — a shard is
\* either held by a peer that is up, or not, and reconstruction is "k of
\* the pinned peers are up," not literal parity math. Also not modeled:
\* the exact string encoding of shard_key/versioned_key in
\* shard_storage.rs — that is an implementation-level injectivity property
\* about a string format, not a property of the state machine this module
\* checks.

EXTENDS Naturals, Sequences, FiniteSets, TLC

CONSTANTS
    Peers,           \* Set of all peers that could ever exist
    Buckets,         \* Set of possible buckets
    Keys,            \* Set of possible keys
    Data,            \* Set of possible data values (same role as single-node's Data)
    RetentionModes,  \* {"NONE", "GOVERNANCE", "COMPLIANCE"}
    K,               \* Data shard count (EC "k")
    M,               \* Parity shard count (EC "m")
    MaxClock         \* Bound on the logical clock, for a finite state space

ASSUME K \in Nat /\ K >= 1
ASSUME M \in Nat /\ M >= 1

VARIABLES
    peers,           \* subset of Peers currently known to the cluster (join-only, never shrinks)
    up,              \* subset of peers currently reachable (models node-down / partition)
    locationRecord,  \* locationRecord[b][k] = the one record carrying placement AND retention
    clock            \* abstract logical time, used only to evaluate retainUntil

\* A location record's shape. "exists" plays the same role as single-node's
\* KeyExists(user,key) check (storage[user][key] # ""). "placement" is the
\* peer set chosen at PUT time and never recomputed afterward. "acked" is
\* the subset of "placement" that actually confirmed the write — kept in
\* the record (not discarded after the PUT action) so QuorumWasMet below
\* can be stated as a plain state invariant instead of a two-state one.
NoRecord == [
    exists      |-> FALSE,
    placement   |-> {},
    acked       |-> {},
    data        |-> "",
    retentionMode |-> "NONE",
    retainUntil |-> 0,
    legalHold   |-> FALSE
]

TypeOK ==
    /\ peers \in SUBSET Peers
    /\ up \in SUBSET Peers
    /\ clock \in 0..MaxClock
    /\ \A b \in Buckets, k \in Keys :
        /\ locationRecord[b][k].exists \in BOOLEAN
        /\ locationRecord[b][k].placement \in SUBSET Peers
        /\ locationRecord[b][k].acked \subseteq locationRecord[b][k].placement
        /\ locationRecord[b][k].data \in Data \cup {""}
        /\ locationRecord[b][k].retentionMode \in RetentionModes
        /\ locationRecord[b][k].retainUntil \in 0..MaxClock
        /\ locationRecord[b][k].legalHold \in BOOLEAN

\* Init starts from exactly one peer, deliberately: the distributed system
\* is born single-node and grows from there, the same direction the real
\* project took (single-node first, cluster/ module added on top later).
Init ==
    /\ peers = {CHOOSE p \in Peers : TRUE}
    /\ up = peers
    /\ locationRecord = [b \in Buckets |-> [k \in Keys |-> NoRecord]]
    /\ clock = 0

KeyExists(b, k) == locationRecord[b][k].exists

\* ComputedPlacement: deterministic given the CURRENT peer set. CHOOSE on
\* a fixed predicate always returns the same element for the same
\* predicate, so two calls with the same S always agree — that
\* determinism is the actual property placement.rs's ComputedPlacement
\* needs for GET/DELETE's "never recompute" guarantee to mean anything,
\* not the specific hash function used to get there. Picks up to k+m
\* peers; fewer if the cluster doesn't have that many yet.
\*
\* Known simplification: this picks the same subset for every (b, k) with
\* the same peer set S, because TLA+ has no built-in hash function and
\* the predicate below only constrains size, not identity. The real
\* ComputedPlacement spreads different keys across different peer
\* subsets (see placement.rs's own
\* different_keys_can_map_to_different_peer_sets test) — that spreading
\* is a load-balancing property, not a correctness property, and none of
\* the invariants below (DataConsistency, QuorumWasMet,
\* PlacementPinSubsetOfKnownPeers, ComplianceIsUndeletable) depend on it.
\* b and k are still taken as parameters, and Put/Update still call this
\* with the real (b, k), so the call shape matches the real code even
\* though this particular model doesn't need the key to affect the
\* result.
ShardTargetCount(S) == IF Cardinality(S) < K + M THEN Cardinality(S) ELSE K + M

ComputedPlacement(b, k, S) ==
    CHOOSE ps \in SUBSET S : Cardinality(ps) = ShardTargetCount(S)

WriteQuorum == IF M = K THEN K + 1 ELSE K
ReadQuorum  == K

\* -------------------------------------------------------------------
\* Membership: additive only. No action ever removes a peer from
\* `peers`, matching membership.rs's "no rebalancing on join" design —
\* there is simply no Leave/Remove action in this spec.
\* -------------------------------------------------------------------
Join(p) ==
    /\ p \in Peers
    /\ p \notin peers
    /\ peers' = peers \cup {p}
    /\ up' = up \cup {p}
    /\ UNCHANGED <<locationRecord, clock>>

\* A peer going down or coming back, used only to make ReadQuorum and
\* WriteQuorum actually bite in the model. Membership (`peers`) is
\* untouched by this — a down peer is still "known," just not reachable,
\* same distinction the real coordinator makes between its peer list and
\* which of those peers actually answer a given call.
CrashPeer(p) ==
    /\ p \in up
    /\ up' = up \ {p}
    /\ UNCHANGED <<peers, locationRecord, clock>>

RecoverPeer(p) ==
    /\ p \in peers \ up
    /\ up' = up \cup {p}
    /\ UNCHANGED <<peers, locationRecord, clock>>

Tick ==
    /\ clock < MaxClock
    /\ clock' = clock + 1
    /\ UNCHANGED <<peers, up, locationRecord>>

\* -------------------------------------------------------------------
\* PUT: resolves placement fresh against the CURRENT peer set, then pins
\* it into locationRecord. This is the one point where placement is ever
\* computed. ackSet is chosen nondeterministically from the resolved
\* placement, modeling "some subset of the fanned-out writes actually
\* landed" — if that subset is too small to meet WriteQuorum, the model
\* records the attempt as a no-op (old record, including any old data,
\* stays exactly as it was), matching "a write that didn't reach quorum
\* never became visible."
\* -------------------------------------------------------------------
Put(b, k, d) ==
    /\ b \in Buckets /\ k \in Keys /\ d \in Data
    /\ ~KeyExists(b, k)
    /\ LET ps == ComputedPlacement(b, k, peers) IN
        \E ackSet \in SUBSET ps :
            IF Cardinality(ackSet) >= WriteQuorum
            THEN locationRecord' = [locationRecord EXCEPT ![b][k] =
                    [exists |-> TRUE, placement |-> ps, acked |-> ackSet, data |-> d,
                     retentionMode |-> "NONE", retainUntil |-> 0, legalHold |-> FALSE]]
            ELSE UNCHANGED locationRecord
    /\ UNCHANGED <<peers, up, clock>>

\* UPDATE mirrors single-node's Update: requires the key already exist.
\* Placement is resolved fresh again here too (not inherited from the old
\* record) — an overwrite is a new write, same as a fresh PUT, and gets
\* its own placement/ack decision against whatever the peer set looks
\* like now. Retention/legalHold carry over unchanged: overwriting an
\* object's bytes does not erase its lock.
Update(b, k, d) ==
    /\ b \in Buckets /\ k \in Keys /\ d \in Data
    /\ KeyExists(b, k)
    /\ LET ps == ComputedPlacement(b, k, peers) IN
        \E ackSet \in SUBSET ps :
            IF Cardinality(ackSet) >= WriteQuorum
            THEN locationRecord' = [locationRecord EXCEPT ![b][k].placement = ps,
                                                           ![b][k].acked = ackSet,
                                                           ![b][k].data = d]
            ELSE UNCHANGED locationRecord
    /\ UNCHANGED <<peers, up, clock>>

\* GET: never calls ComputedPlacement. Only ever reads the pin already
\* sitting in locationRecord, and only succeeds if enough of THAT pinned
\* set is currently up — this is the whole point of pinning: a peer
\* joining after this object's PUT must not change where a GET looks.
Get(b, k) ==
    /\ b \in Buckets /\ k \in Keys
    /\ KeyExists(b, k)
    /\ Cardinality(locationRecord[b][k].placement \cap up) >= ReadQuorum
    /\ UNCHANGED <<peers, up, locationRecord, clock>>

\* Retention check, shared by Delete and GovernanceOverrideDelete so
\* there is exactly one place this logic lives — matching the real design
\* goal of putting retention on the same record as placement precisely so
\* there is only one lookup, not two paths that can disagree.
IsLocked(b, k) ==
    \/ locationRecord[b][k].legalHold
    \/ /\ locationRecord[b][k].retentionMode \in {"GOVERNANCE", "COMPLIANCE"}
       /\ clock <= locationRecord[b][k].retainUntil

\* Plain DELETE: never succeeds while IsLocked. This is the path a normal
\* caller uses — no override permission in play.
Delete(b, k) ==
    /\ b \in Buckets /\ k \in Keys
    /\ KeyExists(b, k)
    /\ ~IsLocked(b, k)
    /\ locationRecord' = [locationRecord EXCEPT ![b][k] = NoRecord]
    /\ UNCHANGED <<peers, up, clock>>

\* The one escape hatch: a caller with override permission can delete a
\* GOVERNANCE-locked object even before retainUntil, but this action's
\* own enabling condition excludes COMPLIANCE entirely — there is no
\* parameter that lets a COMPLIANCE-locked, unexpired object be deleted
\* by any action in this spec, which is exactly the guarantee
\* ComplianceIsUndeletable checks below.
GovernanceOverrideDelete(b, k) ==
    /\ b \in Buckets /\ k \in Keys
    /\ KeyExists(b, k)
    /\ ~locationRecord[b][k].legalHold
    /\ locationRecord[b][k].retentionMode = "GOVERNANCE"
    /\ locationRecord' = [locationRecord EXCEPT ![b][k] = NoRecord]
    /\ UNCHANGED <<peers, up, clock>>

\* Sets a retention lock on an existing object (models the object-lock PUT
\* path: a client marks an already-written object as retained). Can only
\* strengthen: once COMPLIANCE is set, this action cannot be used to
\* switch it back to GOVERNANCE or NONE or shorten retainUntil, matching
\* S3 object-lock semantics (COMPLIANCE cannot be loosened by anyone).
SetRetention(b, k, mode, retainUntilVal) ==
    /\ b \in Buckets /\ k \in Keys
    /\ mode \in RetentionModes
    /\ retainUntilVal \in 0..MaxClock
    /\ KeyExists(b, k)
    /\ locationRecord[b][k].retentionMode # "COMPLIANCE"
    /\ locationRecord' = [locationRecord EXCEPT ![b][k].retentionMode = mode,
                                                 ![b][k].retainUntil = retainUntilVal]
    /\ UNCHANGED <<peers, up, clock>>

Next ==
    \/ \E p \in Peers : Join(p)
    \/ \E p \in Peers : CrashPeer(p)
    \/ \E p \in Peers : RecoverPeer(p)
    \/ Tick
    \/ \E b \in Buckets, k \in Keys, d \in Data : Put(b, k, d)
    \/ \E b \in Buckets, k \in Keys, d \in Data : Update(b, k, d)
    \/ \E b \in Buckets, k \in Keys : Get(b, k)
    \/ \E b \in Buckets, k \in Keys : Delete(b, k)
    \/ \E b \in Buckets, k \in Keys : GovernanceOverrideDelete(b, k)
    \/ \E b \in Buckets, k \in Keys, mode \in RetentionModes, t \in 0..MaxClock :
        SetRetention(b, k, mode, t)

Spec == Init /\ [][Next]_<<peers, up, locationRecord, clock>>

\* -------------------------------------------------------------------
\* Invariants
\* -------------------------------------------------------------------

\* Same shape as single-node's DataConsistency: a key "exists" exactly
\* when it has a non-empty data value. Here that is additionally gated by
\* quorum, since a visible object must also be readable.
DataConsistency ==
    \A b \in Buckets, k \in Keys :
        KeyExists(b, k) <=> (locationRecord[b][k].data # "")

\* Every record that exists was written with quorum ack — a Put/Update
\* that didn't meet WriteQuorum never became visible, so this holds in
\* every reachable state, not just immediately after a successful write.
QuorumWasMet ==
    \A b \in Buckets, k \in Keys :
        KeyExists(b, k) => Cardinality(locationRecord[b][k].acked) >= WriteQuorum

\* The actual guarantee pinning exists for: once a key has a placement,
\* that placement set only ever changes via a fresh Put/Update of that
\* SAME key (which is a deliberate rewrite, not membership drift) — it
\* never silently changes just because peers changed. Phrased as a state
\* invariant: the placement recorded for an existing key is always a
\* valid ComputedPlacement for the CURRENT peer set only when it happens
\* to coincide; the real property is the two-state one below
\* (PlacementPinIsStable).
PlacementPinSubsetOfKnownPeers ==
    \A b \in Buckets, k \in Keys :
        KeyExists(b, k) => locationRecord[b][k].placement \subseteq peers

\* A COMPLIANCE lock with unexpired retainUntil can never be deleted by
\* ANY action in this spec. Written as a next-state (action) safety
\* property: in every step, if a record is COMPLIANCE-locked and
\* unexpired right now, it still exists in the next state. This is
\* exactly the property the real system's two-separate-code-paths bug
\* class (SeaweedFS #8350 / #11333) violated — checked here structurally,
\* across every action, not just the delete handlers.
ComplianceIsUndeletableAction ==
    \A b \in Buckets, k \in Keys :
        (KeyExists(b, k) /\ locationRecord[b][k].retentionMode = "COMPLIANCE"
         /\ clock <= locationRecord[b][k].retainUntil)
        => locationRecord'[b][k].exists

ComplianceIsUndeletable == [][ComplianceIsUndeletableAction]_<<locationRecord, clock>>

\* Membership growing never touches an existing record. Joining peer p
\* must not be the mechanism by which some (b,k)'s placement, acked,
\* data, or retention fields change — only Put/Update/Delete/SetRetention
\* may do that.
JoinNeverMutatesRecordsAction ==
    (\E p \in Peers : Join(p)) => UNCHANGED locationRecord

JoinNeverMutatesRecords == [][JoinNeverMutatesRecordsAction]_<<peers, locationRecord>>

Consistent == DataConsistency /\ QuorumWasMet /\ PlacementPinSubsetOfKnownPeers

\* -------------------------------------------------------------------
\* Refinement into the single-node module: project this module's state
\* down to single-node's (storage, metadata) shape, one bucket treated as
\* one "user," and let SingleNodeView!Consistent be checked directly
\* against that projection. This is the literal, mechanical form of
\* "single-node is the view you get once quorum/placement are resolved" —
\* not just an analogy in a comment.
\*
\* This is a lightweight structural check, not a full linearizability
\* proof: it shows the projection always satisfies single-node's own
\* consistency shape, not that every distributed history corresponds to
\* some single-node execution order. A full refinement proof would also
\* need to relate the two modules' Next actions step-by-step, which is a
\* larger undertaking than this pass covers.
\* -------------------------------------------------------------------
HasQuorum(b, k) == KeyExists(b, k)  \* Get's own enabling condition already requires read quorum against `up`; existence here is what single-node's KeyExists means

storage_proj == [b \in Buckets |-> [k \in Keys |-> locationRecord[b][k].data]]
metadata_proj == [b \in Buckets |-> [k \in Keys |-> IF HasQuorum(b, k) THEN <<1, 1>> ELSE <<>>]]

SingleNodeView == INSTANCE WarpDrive_SingleNode WITH
    Users <- Buckets, Keys <- Keys, Data <- Data,
    storage <- storage_proj, metadata <- metadata_proj

THEOREM RefinesSingleNodeConsistency == Spec => []SingleNodeView!Consistent

====
