# User Guide

> **Note:**  
> These instructions assume you have installed and are running Warpdrive locally.  
> For setup instructions, see the [Developer's Documentation](../docs/setup.md).  
> Once our cloud offering is available, we will update this guide with details for connecting to the managed service. However if you are a developer we suggest you follow our [Developer's Documentation](../docs/setup.md) which is self contained to get you started.  

## Getting Started

Warpdrive is S3-compatible. Any client or library that speaks S3 follows the same semantics. The example below uses **boto3**, and works identically whether the server is running single-node or as a [distributed cluster](#distributed-mode-default) — same URL, same client code either way; which engine answers it is a server-startup decision, not something the client specifies.

```bash
pip install boto3
```

### Configuration

```python
import boto3

s3 = boto3.client(
    's3',
    endpoint_url='http://localhost:9710',
    aws_access_key_id='adminkey',
    aws_secret_access_key='adminsecretkey123456',
    region_name='us-east-1',
    config=boto3.session.Config(s3={'addressing_style': 'path'}),
)
```

Two things to note when running locally:
- **Path-style addressing** (`addressing_style: 'path'`): required because virtual-hosted style (`bucket.localhost`) does not resolve on a local machine.
- **Plain HTTP** (`http://`): TLS is not configured for local development, use `https://` only when connecting to a hosted instance.

---

## Demo and Examples

Ready to try WarpDrive? Check out our comprehensive demos:

### 🚀 **Quick Start Demo**
- **Location**: [`demo/`](../demo/)
- **Features**: Complete S3 compatibility test with 13 different file types
- **Run**: `python3 s3_comprehensive_test.py`

### 📁 **Test Files**
- **Location**: [`demo/test_files/`](../demo/test_files/)
- **Contents**: Sample videos, images, documents, and binary files for testing

### 🐍 **Python Client Demo**
- **Location**: [`demo/pythonTestClient.py`](../demo/pythonTestClient.py)
- **Features**: Native WarpDrive API examples

### 🔧 **Development Setup**
For advanced usage and development details, see the [Developer's Documentation](../docs/setup.md).

---

## Distributed Mode (default)

Everything above is single-node mode, which still exists but is no longer the
recommended way to run WarpDrive — see
[`docs/setup.md`](setup.md#3-run-a-cluster) to get a local cluster running in
one command. This section covers the distributed engine itself: a multi-node,
erasure-coded cluster, S3-compatible end to end. Full architecture, diagrams,
and measured results: [Technical Architecture](Technical-Architecture.md) and
[v1.0.0 Results](benchmarks/v1.0.0-results.md).

### Starting a cluster

Each node is the same binary. The project's own default erasure code is
`RS(3,2)` (5 nodes, 3 data + 2 parity shards per object) — `WARPDRIVE_RS_K`/
`WARPDRIVE_RS_M` default to `3`/`2` if unset, so this is what you get from
just setting `WARPDRIVE_PEERS` with no RS env vars. It tolerates 2 nodes
down for both reads *and* writes, since its write quorum is `k=3`, reachable
with only 3 of 5 nodes up. Every node needs the *same* peer list, including
itself:

```bash
WARPDRIVE_PEERS=http://node1:9710,http://node2:9710,http://node3:9710,http://node4:9710,http://node5:9710 \
WARPDRIVE_ADMIN_ACCESS_KEY=adminkey WARPDRIVE_ADMIN_SECRET_KEY=adminsecretkey123456 \
  ./warp_drive
```

Locally, use [`scripts/run_local_cluster.sh`](../scripts/run_local_cluster.sh)
instead of wiring this up by hand (see `setup.md`).

A smaller, 2-node cluster is also possible: `RS(1,1)` (1 data shard + 1
parity shard — full mirroring, set `WARPDRIVE_RS_K=1 WARPDRIVE_RS_M=1`).
**This tolerates 1 node down for reads only** — WarpDrive's write quorum is
`k+1` whenever `m == k` (`required_write_acks` in `coordinator.rs`, matching
MinIO's own rule for this edge case), which for `RS(1,1)` means both nodes
must be up for a PUT or DELETE to succeed. There's no "1-node cluster":
erasure coding always needs at least 1 parity shard to compute anything, so
a single, unreplicated node is single-node mode (see "Getting Started"
above), not a degenerate cluster.

A node can also join a running cluster later instead of being listed up
front — either by having an existing node announce it:

```bash
curl -X POST http://node1:9710/cluster/join -d '{"peer": "http://node6:9710"}' -H "Content-Type: application/json"
```

or by having the new node join itself at startup, which is the usual way to
add a node on a separate host: set `WARPDRIVE_JOIN_VIA` (any existing
cluster member's address) and `WARPDRIVE_SELF_ADDR` (this node's own address,
as other nodes should reach it — can't be inferred from a `0.0.0.0` bind)
together:

```bash
WARPDRIVE_JOIN_VIA=http://node1:9710 \
WARPDRIVE_SELF_ADDR=http://node6:9710 \
  ./warp_drive
```

### Node address and port configuration

- `WARPDRIVE_PORT` — this node's listen port (default `9710`). The gRPC and
  raw-TCP shard transports run alongside it on `port+1000` and `port+2000`.
- `WARPDRIVE_SELF_ADDR` — the URL other nodes use to reach this one. Required
  alongside `WARPDRIVE_JOIN_VIA` for self-join at startup; set it to whatever
  address is actually reachable from the rest of the cluster (a LAN IP, a
  hostname, etc.) — it is never inferred from the bind address.
- `WARPDRIVE_PEERS` — the seed peer list (self included), comma-separated.
- `WARPDRIVE_PEER_TRANSPORT` — `grpc` (default), `http`, or `tcp`, for the
  peer-to-peer shard transport.

### Basic PUT/GET/DELETE

The cluster API mirrors the single-node API, under `/cluster/{bucket}/{key}` instead of the S3 paths:

```bash
curl -X PUT http://localhost:9710/cluster/mybucket/myobject --data-binary @file.bin
curl http://localhost:9710/cluster/mybucket/myobject -o file.bin
curl -X DELETE http://localhost:9710/cluster/mybucket/myobject
```

Any node can serve any request, there's no dedicated coordinator to route to: a PUT's placement pin is replicated to every known peer (quorum-acked, so one briefly-unreachable peer doesn't fail an otherwise-healthy write), not just the peers holding shard data. Placement, erasure coding (Reed-Solomon), and quorum reads/writes all happen automatically, with no extra configuration required.

### Content-dependent placement (optional, per-bucket)

By default, an object is erasure-coded as a single whole-object stripe. A bucket can opt into placement that understands a workload's own internal structure instead:

```bash
curl -X PUT http://localhost:9710/cluster/_admin/bucket_config/mybucket \
  -H "Content-Type: application/json" \
  -d '{"packer_name": "fac", "overhead_threshold_pct": 100.0}'
```

`packer_name` is `"fac"` (size-based bin-packing, good for columnar formats like Parquet) or `"ivf_centroid"` (groups by similarity, good for vector-index partitions). Then PUT with an `x-warpd-computable-units` header describing the object's real internal byte ranges:

```bash
curl -X PUT http://localhost:9710/cluster/mybucket/data.parquet \
  --data-binary @data.parquet \
  -H 'x-warpd-computable-units: [[0, 1024, 1024, "opaque"], [1024, 2048, 2048, "opaque"]]'
```

Each entry is `[offset, length, length, codec]` in the original object's byte layout. If the resulting packed overhead exceeds `overhead_threshold_pct`, WarpDrive falls back to plain whole-object storage automatically rather than risk availability for a bad packing decision.

### Authenticated S3 access (recommended for S3 clients and third-party tools)

A standard S3 client (boto3, aws-cli, Lance's `object_store::aws`,
pyiceberg, Spark, etc.) should use the regular `/s3/{bucket}/{key}` (or
root `/{bucket}/{key}`) endpoint — the exact same one single-node mode
uses. Which engine actually answers it is decided once, at process
startup, by whether `WARPDRIVE_PEERS` was set; the URL and client code
never change. Unlike the bare `/cluster/{bucket}/{key}` routes above,
this requires real SigV4 authentication (the same verification
single-node's API uses) and supports GET/PUT/HEAD/DELETE/LIST and full
multipart upload on top of the erasure-coded cluster engine, with real
`ETag`/`Last-Modified`/user-metadata round-tripped from the stored
object. The bare `/cluster/{bucket}/{key}` routes stay unauthenticated,
for quick local testing (`curl`) without signing requests.

```python
s3 = boto3.client(
    's3',
    endpoint_url='http://localhost:9710/s3',
    aws_access_key_id='adminkey',
    aws_secret_access_key='adminsecretkey123456',
    region_name='us-east-1',
    config=boto3.session.Config(s3={'addressing_style': 'path'}),
)
```

`/cluster/s3/{bucket}/{key}` still works too, unchanged — it's the same
cluster-authenticated handlers, kept as an explicit, always-cluster alias
for existing tooling (the Jepsen/chaos test harnesses target it directly)
and for forcing cluster access specifically if that's ever useful. New
code should just use `/s3/...`.

Bucket lifecycle (create/delete/head-bucket, bucket-level config like
`?cors`/`?versioning`) stays single-node-only in both modes: the cluster
engine has no bucket registry to hang that on (a deliberate,
already-documented scope limit, not new here). `CreateBucket` "succeeds"
against single-node's own local metadata regardless of which engine is
actually serving object traffic — harmless, since cluster PUT/GET never
checks bucket existence, but worth knowing if you're inspecting what a
`CreateBucket` call actually touched.

Not yet supported on this surface: bucket-level CORS/ACL and object
versioning (no cluster bucket registry exists for the former; versioning
is a stated non-goal) — object tagging and retention/legal-hold work the
same as single-node.

### Read more

- [Technical Architecture](Technical-Architecture.md): write-path and read-path diagrams, quorum rules, membership
- [v1.0.0 Results](benchmarks/v1.0.0-results.md): measured speedups from content-dependent placement against DuckDB (TPC-H) and Lance (vector search)
