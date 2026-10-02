# WarpDrive Architecture Documentation

## Introduction

WarpDrive is a high-throughput key-value/object store optimized for Storage Disaggregated Architectures and AI/ML workloads. The implementation is based on Facebook's 2008 Haystack paper,
focusing on efficient storage and retrieval of objects through a simplified architecture.

As of **v1.0.0**, WarpDrive runs in two modes:

- **Single-node mode** — a single process, fully S3-compatible, for local
  deployments and development. This is the original architecture below,
  unchanged.
- **Distributed mode** — a real, multi-node, erasure-coded object engine with
  pluggable placement, built as a layer in front of the same single-node
  storage/API code (every peer is still a single-node server underneath).
  See [Distributed Mode](#distributed-mode---v100) below, and the full
  real-measurement results in
  [`docs/benchmarks/v1.0.0-real-results.md`](benchmarks/v1.0.0-real-results.md).

## Single-Node Mode (fully S3-compatible, for local deployments) - v0.1.0


```mermaid
graph TD
    subgraph "Storage Service"
        subgraph "API Layer"
            API[API Server :9710]
            NATIVE[Native CIAOS API]
            S3API[S3-Compatible API]
        end

        subgraph "Request Processing"
            SVC[Service Layer]
            S3HANDLER[S3 Handlers]
            UNIFIED[Unified Storage Interface]
            BIN[Binary Storage]
            META[Metadata Storage]
        end

        subgraph "Storage Implementation"
            XFS[XFS File System]
            DB[(SQLite Database)]
        end

        API -->|Native Requests| NATIVE
        API -->|S3 Requests| S3API
        NATIVE -->|Process Request| SVC
        S3API -->|Process S3 Request| S3HANDLER
        SVC -->|Unified Storage| UNIFIED
        S3HANDLER -->|Unified Storage| UNIFIED
        UNIFIED -->|Store File Data| BIN
        UNIFIED -->|Store Metadata| META
        BIN -->|Single Binary File per User| XFS
        META -->|Key -> Offset/Size Mapping| DB
    end

    C[Client] -->|HTTP Requests| API
    S3CLIENT[S3 Client] -->|boto3/aws-cli| S3API
```

## Distributed Mode - v1.0.0

Every deployed node is **symmetric** — the same binary, both a storage node
(the single-node architecture above, unchanged) and a coordinator that can
accept a request, decide placement, and fan out to peers. There is no
dedicated coordinator process and no leader election: consistent with how
MinIO and SeaweedFS run in production, placement is a deterministic function
of `(bucket, key)`, and consistency comes from a read/write quorum, not
consensus.

```mermaid
graph TD
    CLIENT[Client / S3 SDK / DuckDB httpfs / Lance object_store] -->|PUT GET DELETE| COORD

    subgraph "Any node, acting as coordinator for this request"
        COORD[cluster/coordinator.rs]
        S3SHIM["cluster/s3_shim.rs\n(GET/PUT/HEAD/List, real bucket semantics\nfor Lance's object_store client)"]
        PLACEMENT{{"PlacementPolicy\n(trait, 1 impl: ComputedPlacement\nrendezvous hash, no leader election)"}}
        BUCKETCFG["BucketConfigStore\n(per-bucket packer choice +\noverhead-threshold fallback)"]
        PACKERS{{"StripePacker registry\n(trait: FacPacker, IvfCentroidPacker,\nuser-pluggable)"}}
        EC{{"ErasureCoder\n(trait, 1 impl: ReedSolomonCoder\nRS(k,m))"}}
        PUSHDOWN["pushdown.rs: ColumnCodec\nin-situ filter, no reassembly"]
        LOC[("LocationStore\nBitcask-style, pinned at PUT")]
        CLOC[("ContentLocationStore\nunit -> stripe -> peers")]
    end

    COORD --> PLACEMENT
    COORD --> BUCKETCFG --> PACKERS
    COORD --> EC
    COORD --> PUSHDOWN
    COORD --> LOC
    COORD --> CLOC
    S3SHIM --> COORD

    COORD -->|gRPC, pooled, concurrent fan-out| P1[Peer node 1\nsingle-node storage]
    COORD -->|gRPC| P2[Peer node 2\nsingle-node storage]
    COORD -->|gRPC| P3["Peer node N\n(k data + m parity shards)"]

    P1 -. "/cluster/join (membership)" .-> COORD
```

**What each piece is, concretely (not aspirational — all shipped and tested):**

- **`PlacementPolicy`** (`placement.rs`) — `ComputedPlacement`, a CRUSH-style
  deterministic hash of `(bucket, key)` to a `k+m` peer set. Resolved fresh at
  PUT time and pinned into `LocationStore`; GET/DELETE read the pin, so a
  node joining or leaving never moves already-placed data.
- **`StripePacker`** (`packing.rs`) — pluggable **content-dependent
  placement**, the opt-in layer on top of the always-on default above.
  `FacPacker` (Fusion's Algorithm 1, size-based bin-packing) and
  `IvfCentroidPacker` (groups by real k-means cluster id, then bin-packs
  within each cluster) are both registered implementations of the same
  trait — a bucket picks one via `BucketConfigStore`, or uses neither and
  gets plain erasure-coded storage.
- **`ErasureCoder`** (`ec.rs`) — `ReedSolomonCoder`, real `reed-solomon-erasure`
  encode/decode, with a second `encode_shards`/`decode_shards` path for
  pre-packed (content-dependent) stripes.
- **`LocationStore` / `ContentLocationStore`** (Bitcask-style: append-only
  log + in-memory index) — the plain object → peer-set pin, and the
  multi-stripe unit → (offset, stripe, peers) record content-dependent
  placement needs, kept as two stores since the record shapes genuinely
  differ.
- **`cluster/s3_shim.rs`** — a minimal real S3 surface (GET/PUT/HEAD/List)
  over the same coordinator path, built specifically so third-party clients
  that need real bucket/list semantics (Lance's `object_store::aws`) work
  unmodified, not just protocol-agnostic range-GET clients (DuckDB's
  `httpfs`, which needs no shim).
- **`pushdown.rs`** — a `ColumnCodec` trait + a `/query` endpoint that filters
  a column in-place on the peer holding it, no whole-object reassembly.
- **Membership** — additive only (SeaweedFS/MinIO-pool style): a new node
  calls `/cluster/join`, is added to the live peer list, and starts taking
  new writes immediately. Already-placed objects are never rebalanced —
  documented non-goal, not an oversight (see
  `docs/Distributed-Engine-Plan.md`).

**Real measured results for content-dependent placement, run against this
exact engine** (real Lance IVF_PQ vector search, real SIFT1M-small benchmark
data, real official TPC-H via DuckDB's own `dbgen` and query set) are in
[`docs/benchmarks/v1.0.0-real-results.md`](benchmarks/v1.0.0-real-results.md).

