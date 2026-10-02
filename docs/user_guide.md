# User Guide

> **Note:**  
> These instructions assume you have installed and are running Warpdrive locally.  
> For setup instructions, see the [Developer's Documentation](../docs/setup.md).  
> Once our cloud offering is available, we will update this guide with details for connecting to the managed service. However if you are a developer we suggest you follow our [Developer's Documentation](../docs/setup.md) which is self contained to get you started.  

## Getting Started

Warpdrive is S3-compatible. Any client or library that speaks S3 follows the same semantics. The example below uses **boto3**.

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

Ready to try CIAOS? Check out our comprehensive demos:

### 🚀 **Quick Start Demo**
- **Location**: [`demo/`](../demo/)
- **Features**: Complete S3 compatibility test with 13 different file types
- **Run**: `python3 s3_comprehensive_test.py`

### 📁 **Test Files**
- **Location**: [`demo/test_files/`](../demo/test_files/)
- **Contents**: Sample videos, images, documents, and binary files for testing

### 🐍 **Python Client Demo**
- **Location**: [`demo/pythonTestClient.py`](../demo/pythonTestClient.py)
- **Features**: Native CIAOS API examples

### 🔧 **Development Setup**
For advanced usage and development details, see the [Developer's Documentation](../docs/setup.md).

---

## Distributed Mode (v1.0.0)

Everything above is single-node mode. WarpDrive can also run as a multi-node, erasure-coded cluster. Full architecture, diagrams, and measured results: [Technical Architecture](Technical-Architecture.md) and [v1.0.0 Results](benchmarks/v1.0.0-results.md).

### Starting a cluster

Each node is the same binary, started with a peer list:

```bash
WARPDRIVE_PEERS=http://node1:9710,http://node2:9710,http://node3:9710,http://node4:9710,http://node5:9710 \
  ./warp_drive
```

A node can also join a running cluster later instead of being listed up front:

```bash
curl -X POST http://node1:9710/cluster/join -d '{"peer": "http://node6:9710"}' -H "Content-Type: application/json"
```

### Basic PUT/GET/DELETE

The cluster API mirrors the single-node API, under `/cluster/{bucket}/{key}` instead of the S3 paths:

```bash
curl -X PUT http://localhost:9710/cluster/mybucket/myobject --data-binary @file.bin
curl http://localhost:9710/cluster/mybucket/myobject -o file.bin
curl -X DELETE http://localhost:9710/cluster/mybucket/myobject
```

Any node can serve any request, there's no dedicated coordinator to route to. Placement, erasure coding (Reed-Solomon), and quorum reads/writes all happen automatically, with no extra configuration required.

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

### S3-shaped access for third-party tools (Lance, etc.)

Tools that need real S3 bucket/list semantics (not just range-GET, e.g. Lance's `object_store::aws` client) can point at `/cluster/s3/{bucket}/{key}` instead, which supports GET/PUT/HEAD/List on top of the same cluster engine.

### Read more

- [Technical Architecture](Technical-Architecture.md): write-path and read-path diagrams, quorum rules, membership
- [v1.0.0 Results](benchmarks/v1.0.0-results.md): measured speedups from content-dependent placement against DuckDB (TPC-H) and Lance (vector search)
