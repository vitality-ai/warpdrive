# RFC 4 - Distributed Fault Tolerance: Quorum-Based Metadata and Data Durability

**Status:** Draft
**Branch:** `feat/rfc4-fault-tolerance`
**Date:** June 2026

---

## Problem

Warpdrive today is a single-node system. Both metadata (SQLite) and data (segment files per RFC3) live on one machine. If that machine fails, everything is unavailable. There is no replication, no redundancy, and no recovery path.

The goal of this RFC is to make a cluster of Warpdrive nodes collectively provide fault tolerance — without introducing external dependencies like etcd, Zookeeper, or a separate block storage service. Each Warpdrive node participates in both metadata consensus and data replication. The result is a self-contained distributed system: one binary, N nodes, tunable durability.

---

## Current Structure

### Metadata (SQLite)

Each node today has a single SQLite file with five tables:

```
objects          — S3 object extents (user, bucket, key, offset_size_list, etag, size, ...)
buckets          — bucket registry (user, name, created_at)
multipart_uploads — in-flight MPU state (upload_id, bucket, key, status, final_etag, ...)
multipart_parts   — per-part records (upload_id, part_number, etag, size, extents_blob)
deletion_queue    — extent ranges queued for background GC (offset_size_list, processed)
```

Writes are serialised through a single `Arc<Mutex<Connection>>`. There is no replication. Reads and writes are strictly local.

### Storage (RFC3 Segment Files)

RFC3 splits per-bucket storage into fixed-size 512 MB segment files:

```
storage/{bucket}/0000000000.seg
storage/{bucket}/0000000001.seg
...
```

The existing `(offset, size)` extents in SQLite implicitly encode the segment file via:

```
segment_id   = offset / 512_MB
local_offset = offset % 512_MB
```

Writes are append-only to the active segment. Compaction (Option A, T = 0.3-0.4) copies survivors from cold segments into the active segment and deletes the cold file. No schema change from RFC3.

---

## Goal

Run N Warpdrive nodes. Any node can serve reads and writes. The cluster tolerates `floor((N-1)/2)` node failures without data loss or unavailability. With N=3, one node can fail and the cluster keeps working.

No external coordinator. No separate metadata service. Each node drives both.

---

## Design

### Cluster Membership

Each node has a static peer list in its config:

```toml
[cluster]
node_id = "node-1"
peers   = ["node-2:9711", "node-3:9711"]
```

This is intentionally simple — no gossip, no dynamic membership for now. Adding a node is an operator action. The peer list defines the quorum group.

### Quorum Parameters

For a cluster of N nodes:

```
W (write quorum) = floor(N/2) + 1
R (read quorum)  = floor(N/2) + 1
```

W + R > N guarantees that any read quorum overlaps any write quorum — at least one node that participated in the write will respond to every read. With N=3: W=2, R=2.

A write is acknowledged to the client only after W nodes have durably committed it. Reads return the value with the highest version seen across R nodes.

---

## Metadata Consensus

### The Core Problem

SQLite is a local store. To make metadata consistent across N nodes, writes must be agreed upon before any node commits them. Without agreement, two nodes could independently accept conflicting writes (e.g., two clients creating the same key simultaneously) and diverge.

### Approach: Single-Leader Replication with Quorum Commit

One node is the **metadata leader**. All metadata writes go to the leader. The leader replicates the write to followers before acknowledging it to the client. A write is committed once W nodes (leader + W-1 followers) have written it to their local SQLite.

```
Client → any node → [forward to leader if not leader]
Leader → write to own SQLite (WAL entry)
Leader → replicate to followers in parallel
Leader → wait for W-1 follower acks
Leader → commit + ack client
```

Reads can be served by any node that has caught up to the committed log position. For strong consistency, reads go through the leader or include a version check.

### Leader Election

On startup, nodes hold a randomised election timeout (150-300ms, similar to Raft). If a follower does not hear from a leader within its timeout, it calls an election. The node with the most up-to-date log wins if it gets votes from a quorum.

This is intentionally Raft-like without implementing full Raft. The invariant we need is simple: at most one leader at a time, and a leader only commits after W nodes have the write. We do not need log compaction snapshots or dynamic membership changes for the initial version.

### What Gets Replicated

Every write to SQLite is serialised as a log entry before being applied:

```
LogEntry {
    term:      u64,      // leader's current term
    index:     u64,      // monotonically increasing
    op:        OpType,   // PutObject, DeleteObject, CreateBucket, ...
    payload:   Vec<u8>,  // serialised operation arguments
}
```

Followers apply entries in order. Their local SQLite is the materialised state of the log up to the committed index. On crash recovery, a node replays unapplied log entries from its WAL before rejoining the cluster.

### Metadata Trait Changes

The `MetadataStorage` trait gains two primitives that enable distributed operation:

```rust
// Conditional write — fails if current version != expected_version
fn cas(&self, bucket: &str, key: &str,
       expected_version: Option<u64>, value: &Metadata) -> Result<u64, Error>;

// Atomic batch — all ops commit or none do
fn txn(&self, ops: Vec<MetadataOp>) -> Result<(), Error>;
```

`CompleteMultipartUpload` uses `txn` to atomically write the final object, delete all part rows, and queue part extents for GC — currently three separate operations that can be half-applied on crash.

---

## Data Replication

### Approach: Primary-Copy Replication

When a client writes an object, the receiving node is the **primary** for that write. The primary:

1. Appends the bytes to its local active segment
2. Streams the same bytes to W-1 **replica** nodes in parallel
3. Each replica appends to its own local segment (which may have a different offset)
4. Each replica acks the primary with its `(node_id, offset, size)`
5. Once W-1 replica acks are received, the primary records **all** extents in a single metadata write:

```
objects row: {
    key: "foo",
    replicas: [
        { node: "node-1", offset: 1073741824, size: 4096 },
        { node: "node-2", offset:  536870912, size: 4096 },
        { node: "node-3", offset: 1610612736, size: 4096 },
    ]
}
```

The metadata commit goes through the leader as described above. The client gets an ack only after both the data quorum and the metadata commit succeed.

### Why Offsets Differ Per Node

Each node has its own segment files. A write that lands at offset `2GB` on node-1 may land at offset `1GB` on node-2 if node-2 had fewer prior writes. The metadata row stores per-node extents — there is no global offset in a multi-node cluster. The global offset concept from RFC3 becomes per-node local.

### Read Path

A read request arrives at any node. The node looks up the metadata row (via the metadata leader or local read if caught up) to find which nodes hold replicas. It picks the local node if it has a replica, otherwise picks the closest available replica. If the chosen node is down, it tries the next replica.

### Data and Metadata Consistency

The metadata commit (step 5) is the commit point. Before that commit, the data bytes exist on W nodes but no client can read them — the metadata row does not yet exist. After the commit, any node serving a read will find the metadata row and be able to locate a live replica. This gives read-after-write consistency.

---

## Failure Scenarios

### Node Failure During Write (Data)

If a replica node fails after receiving bytes but before acking, the primary does not reach write quorum with that node. It either retries the failed node or proceeds with the remaining nodes if W is still reachable without it. If W is not reachable, the write fails and the client gets an error — no partial commit.

The bytes written to the failed node's segment are dead (no metadata row was committed). On recovery, the failed node's deletion worker will not find them in the `objects` table and they remain as dead space until compaction reclaims them.

### Node Failure During Write (Metadata)

If the metadata leader fails mid-replication, the uncommitted log entry is lost. The new leader starts fresh from the last committed index. The client's write fails. The data bytes written to segment files across nodes are similarly stranded — dead space, reclaimed by compaction.

### Node Failure at Rest

A node that goes down at rest has its replicas still available on the surviving nodes (assuming W >= 2). Reads continue unaffected. Writes continue as long as quorum is reachable. The failed node's data is stale when it comes back — it replays the metadata log to catch up.

---

## What Does Not Change

- **Segment file layout** — RFC3's 512 MB segments, Option A compaction, T=0.3-0.4. Each node runs its own compactor independently. Compaction is local.
- **S3 API surface** — handlers are unchanged. The quorum operations are below the `MetadataService` and `StorageService` seams.
- **SQLite** — still the local metadata store on each node. The consensus layer sits above it, replicating log entries and applying them locally.

---

## Open Questions

1. **Log storage**: the replication log needs to be durable itself. Does it live in SQLite (a `wal_log` table) or a separate append-only file? SQLite WAL mode already provides durability — a `wal_log` table is the simplest starting point.
2. **Replica placement**: with N=3 nodes, all nodes always hold all data. With larger N, do we want to place replicas on a subset of nodes per object? That is a future problem (shard-aware placement).
3. **Re-replication**: when a node comes back after failure, its data is stale. For metadata, replay the log. For data, we need a repair path — copy missing extents from surviving replicas. Not designed here.
4. **Compaction coordination**: if two nodes independently compact the same cold segment at the same time, no conflict occurs (they each compact their own local copy). But if one node's compaction is ahead of another's, reads could land on a stale replica that no longer has the old extent. The read path must check the replicated metadata row's extents and retry on a replica that has caught up.

---

## Summary

| Layer | Today | RFC4 |
|---|---|---|
| Metadata store | Local SQLite, single node | SQLite on each node, leader-replicated log |
| Metadata write | Local mutex | Quorum commit via leader (W=2 of 3) |
| Data store | Local segment files | Segment files on each node, primary-copy replication |
| Data write | Append to local segment | Append to W nodes in parallel, commit metadata after quorum ack |
| Failure tolerance | None | 1 node failure with N=3 |
| External dependencies | None | None |
