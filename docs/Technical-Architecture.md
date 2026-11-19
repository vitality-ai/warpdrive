# WarpDrive Architecture Documentation

## Introduction

WarpDrive is a high-throughput key-value/object store optimized for Storage Disaggregated Architectures and AI/ML workloads. The implementation is based on Facebook's 2008 Haystack paper,
focusing on efficient storage and retrieval of objects through a simplified architecture.

As of **v1.0.0-beta**, WarpDrive runs in two modes:

- **Single-node mode**: a single process, fully S3-compatible, for local
  deployments and development. This is the original architecture below,
  unchanged.
- **Distributed mode**: a multi-node, erasure-coded object engine with
  pluggable placement, built as a layer in front of the same single-node
  storage/API code (every peer is still a single-node server underneath).
  See [Distributed Mode](#distributed-mode---v100) below, and the measured
  results in
  [`docs/benchmarks/v1.0.0-results.md`](benchmarks/v1.0.0-results.md).

## Code modules and how they talk

Every node runs the same `warp_drive` binary (`server/src/`). The router in
`main.rs` sends each S3 request to one of two engines. A node with no peers
uses the single-node engine. A node with peers uses the cluster engine,
whose nodes talk to each other directly. Arrows point from the caller to
the module it calls.

```mermaid
flowchart TB
    CLIENT(["S3 client<br/>boto3, aws-cli, SDKs"])

    subgraph ENTRY["HTTP entry · main.rs"]
        ROUTER["actix-web router"]
        PICK{"pick_engine<br/>any peers?"}
    end

    subgraph S3["S3 API · s3/"]
        AUTH["auth.rs<br/>SigV4, presigned URLs"]
        STS["sts.rs<br/>AssumeRoleWithWebIdentity"]
        HANDLERS["handlers/<br/>object, bucket, listing, multipart,<br/>versioning, acl, cors, tagging,<br/>object_lock, copy, post_object"]
    end

    subgraph SN["Single-node engine · service/"]
        MS["metadata_service.rs"]
        SS["storage_service.rs"]
        DW["deletion_worker.rs<br/>background"]
        API["api.rs<br/>native /put /get"]
    end

    subgraph META["Metadata · metadata/"]
        MTRAIT["MetadataStorage trait"]
        SQLITE[("sqlite_store.rs<br/>SQLite")]
    end

    subgraph CL["Cluster engine · cluster/"]
        SURF["s3_surface.rs<br/>cluster S3 handlers"]
        COORD["coordinator.rs<br/>PUT, GET, DELETE, quorum"]
        PLACE["placement.rs<br/>rendezvous hash"]
        PACK["packing.rs + packed.rs<br/>content-dependent stripes"]
        EC["ec.rs<br/>Reed-Solomon"]
        PUSH["pushdown.rs<br/>column filters"]
        MEMB["membership.rs<br/>peer list, join"]
        CMS["metadata_store.rs<br/>read-only MetadataStorage"]
    end

    subgraph CST["Cluster stores · append-only log + in-memory index"]
        LOC["location_store.rs<br/>where each object lives"]
        CLOC["content_location_store.rs<br/>packed objects"]
        BCFG["bucket_config.rs<br/>per-bucket placement"]
        SMETA["shard_meta_store.rs<br/>where each shard sits on disk"]
    end

    subgraph TR["Peer transport · cluster/"]
        PC["peer_client.rs<br/>PeerClient trait + HTTP client"]
        GRPC["grpc_peer_client.rs<br/>default · port + 1000"]
        TCP["tcp_peer_client.rs<br/>port + 2000"]
        SHARD["shard_storage.rs<br/>stores received shards"]
    end

    subgraph STORE["Storage · storage/"]
        STRAIT["Storage trait"]
        LOCAL["local_store.rs<br/>one append-only file per bucket"]
        GC["group_commit.rs + durable_log.rs<br/>batched fsync"]
    end

    CLIENT --> ROUTER
    ROUTER --> PICK
    ROUTER -->|"/sts"| STS
    ROUTER -->|"bucket create, delete, head,<br/>list buckets: both modes"| HANDLERS
    PICK -->|"no peers"| HANDLERS
    PICK -->|"peers"| SURF

    HANDLERS --> AUTH
    SURF --> AUTH
    HANDLERS --> MS
    HANDLERS --> SS
    API --> SS
    DW --> MS
    DW --> SS
    MS --> MTRAIT --> SQLITE
    SS --> STRAIT --> LOCAL --> GC
    LOCAL -.->|"queue freed space"| SQLITE

    SURF --> COORD
    SURF -->|"HEAD, conditional PUT"| CMS
    SURF -->|"listing"| LOC
    CMS --> LOC
    CMS --> CLOC
    COORD --> PLACE
    COORD --> PACK
    COORD --> EC
    COORD --> PUSH
    COORD --> MEMB
    COORD --> BCFG
    COORD --> LOC
    COORD --> CLOC
    COORD -->|"shards"| PC
    PC --> GRPC
    PC --> TCP
    GRPC --> SHARD
    TCP --> SHARD
    SHARD --> SMETA
    SHARD --> STRAIT
    LOC --> GC
    CLOC --> GC
    BCFG --> GC
    SMETA --> GC

    classDef entry fill:#eef0f3,stroke:#6b7280,stroke-width:2px,color:#111827
    classDef s3 fill:#dbeafe,stroke:#2a78d6,stroke-width:2px,color:#0b2540
    classDef single fill:#d9f5e8,stroke:#1baf7a,stroke-width:2px,color:#06301f
    classDef meta fill:#fdf0c8,stroke:#eda100,stroke-width:2px,color:#3a2800
    classDef cluster fill:#ece6fd,stroke:#7c3aed,stroke-width:2px,color:#2a0f63
    classDef cstore fill:#fbe3ef,stroke:#db2777,stroke-width:2px,color:#4a0a28
    classDef transport fill:#d5f5f1,stroke:#0d9488,stroke-width:2px,color:#053a35
    classDef storage fill:#fde5d6,stroke:#eb6834,stroke-width:2px,color:#4a1a03
    class CLIENT,ROUTER,PICK entry
    class AUTH,STS,HANDLERS s3
    class MS,SS,DW,API single
    class MTRAIT,SQLITE meta
    class SURF,COORD,PLACE,PACK,EC,PUSH,MEMB,CMS cluster
    class LOC,CLOC,BCFG,SMETA cstore
    class PC,GRPC,TCP,SHARD transport
    class STRAIT,LOCAL,GC storage
```

| Color | Module | What it does |
|---|---|---|
| gray | `main.rs` | Routes every HTTP request. `pick_engine` sends S3 object requests to the cluster engine when the node has peers, and to the single-node engine when it has none. A node configured for a cluster that hasn't joined yet answers 503. Bucket create, delete and head go to the single-node handlers in both modes. |
| blue | `s3/` | SigV4 and presigned-URL auth (`auth.rs`), STS federation (`sts.rs`), and the single-node S3 handlers (`handlers/`). |
| green | `service/` | The single-node engine: `MetadataService` and `StorageService` behind the handlers, and the background `deletion_worker`. `api.rs` is the native `/put`, `/get` API. It's deprecated for clients, but the HTTP peer transport still uses it. |
| amber | `metadata/` | The `MetadataStorage` trait and its SQLite implementation: buckets, versions, ACLs, multipart uploads, Object Lock and tags. |
| purple | `cluster/` engine | `s3_surface.rs` is the cluster's S3 front end. `coordinator.rs` runs every PUT, GET and DELETE. It picks peers (`placement.rs`), erasure codes (`ec.rs`), packs stripes for content-dependent buckets (`packing.rs`, `packed.rs`), and checks quorums. `metadata_store.rs` exposes the location stores as a read-only `MetadataStorage` for HEAD and conditional PUTs. Listing reads the location stores directly. |
| pink | `cluster/` stores | Records replicated to every node, each an append-only log with an in-memory index: object locations, packed-object locations and bucket placement config. `shard_meta_store.rs` is per node: where each shard it holds sits on disk. |
| teal | `cluster/` transport | `PeerClient` moves shards between nodes over gRPC (default), raw TCP, or HTTP (`WARPDRIVE_PEER_TRANSPORT`). Each node runs the gRPC and TCP shard servers, which store what they receive through `shard_storage.rs`. |
| orange | `storage/` | The `Storage` trait and `local_store.rs`, one append-only file per user and bucket. Both engines write object bytes through it. `group_commit.rs` and `durable_log.rs` batch fsyncs for the stores and logs. |

### How nodes talk to each other

Any node can coordinate a request. The coordinator sends shards to the
nodes that hold them, and sends location records to every node.

```mermaid
flowchart LR
    C(["S3 client"]) -->|"PUT, GET<br/>/s3/bucket/key"| A_COORD

    subgraph A["Node A · coordinates this request"]
        A_COORD["coordinator.rs"]
        A_PC["PeerClient"]
        A_COORD --> A_PC
    end

    subgraph B["Node B · each peer"]
        B_SHARD["shard_storage.rs"]
        B_STORE["local_store.rs<br/>+ shard_meta_store.rs"]
        B_INT["coordinator.rs<br/>internal handlers"]
        B_LOC["location stores"]
        B_MEMB["membership.rs"]
        B_PUSH["pushdown.rs"]
        B_SHARD --> B_STORE
        B_INT --> B_LOC
    end

    A_PC ==>|"gRPC PutShard, GetShard<br/>port + 1000 (default)"| B_SHARD
    A_PC -.->|"raw TCP, port + 2000<br/>or HTTP /put, /get"| B_SHARD
    A_COORD ==>|"HTTP POST, DELETE<br/>/cluster/_internal/location,<br/>content_location, bucket_config"| B_INT
    A_COORD -->|"HTTP POST<br/>/cluster/_internal/pushdown_filter"| B_PUSH
    NEW(["joining node"]) -->|"HTTP POST /cluster/join"| B_MEMB

    classDef client fill:#eef0f3,stroke:#6b7280,stroke-width:2px,color:#111827
    classDef cluster fill:#ece6fd,stroke:#7c3aed,stroke-width:2px,color:#2a0f63
    classDef transport fill:#d5f5f1,stroke:#0d9488,stroke-width:2px,color:#053a35
    classDef cstore fill:#fbe3ef,stroke:#db2777,stroke-width:2px,color:#4a0a28
    classDef storage fill:#fde5d6,stroke:#eb6834,stroke-width:2px,color:#4a1a03
    class C,NEW client
    class A_COORD,B_INT,B_MEMB,B_PUSH cluster
    class A_PC,B_SHARD transport
    class B_LOC cstore
    class B_STORE storage
```

- **Shards** go only to the `k+m` nodes that placement picked. A write
  succeeds when `k` of them acknowledge (`k+1` when `m == k`). A read needs
  any `k`.
- **Location records** go to every node, with the same write quorum, so any
  node can find any object without asking another node first.
- **Membership** is additive. A joining node calls `/cluster/join` on a
  peer, gets the peer list back, and announces itself to the rest.
- The `/cluster/_internal/*` routes and the shard ports aren't authenticated
  yet (#157). Keep them on a private network.

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

### Request sequence (PUT and GET)

```mermaid
sequenceDiagram
    participant C as S3 Client
    participant API as API Server :9710
    participant AUTH as SigV4 auth
    participant SVC as StorageService
    participant META as MetadataService
    participant BIN as Binary Storage
    participant DB as SQLite

    C->>API: PUT /bucket/key (SigV4-signed)
    API->>AUTH: authenticate_s3_request
    AUTH-->>API: ok, or 403
    API->>SVC: write_object(body)
    SVC->>BIN: append to this user's single binary file
    BIN-->>SVC: (offset, size)
    SVC->>META: write_metadata(bucket, key, offset, size)
    META->>DB: INSERT key -> offset/size
    API-->>C: 200 OK

    C->>API: GET /bucket/key (SigV4-signed)
    API->>AUTH: authenticate_s3_request
    API->>META: read_metadata(bucket, key)
    META->>DB: SELECT offset/size WHERE key
    DB-->>META: (offset, size), or none -> 404
    API->>SVC: read_object(offset, size)
    SVC->>BIN: read that byte range of the binary file
    BIN-->>SVC: bytes
    API-->>C: 200/206 + bytes
```

Real SigV4 signature verification on every request is exactly what the
distributed mode's minimal `/cluster/s3/` surface (below) does **not**
have. One key, one binary file per user, one metadata row per object. No
quorum, no placement, no erasure coding to reason about here.

## Distributed Mode - v1.0.0-beta

Correctness review log: [`docs/reviews/v1.0.0-beta-code-review.md`](reviews/v1.0.0-beta-code-review.md).

Every deployed node is **symmetric**: the same binary, both a storage node
(the single-node architecture above, unchanged) and a coordinator that can
accept a request, decide placement, and fan out to peers. There is no
dedicated coordinator process and no leader election: consistent with how
MinIO and SeaweedFS run in production, placement is a deterministic function
of `(bucket, key)`, and consistency comes from a read/write quorum, not
consensus.

The write path and the read path are deliberately asymmetric, so it's worth
looking at them separately, plus a third diagram for how a bucket's
placement/packer choice actually gets wired up across the cluster's nodes.

### Cluster membership and per-bucket placement configuration

Three steps, each building on the last: a node joins the cluster, a bucket's
config picks a placement policy, and that config's `packer_name` selects one
entry from the `StripePacker` registry.

**1. A new node joins the running cluster.** No leader election, no
consensus. The joining node announces itself to every peer it currently
knows about over plain HTTP, best-effort: a peer that's briefly
unreachable during that round simply doesn't get the update, and that gap
isn't automatically repaired later (no gossip, no retry). In practice
(all peers reachable) every node converges on the same list; the failure
mode is a split list after a missed announce, not a hang or an error.

![Cluster membership: additive only, no leader election](diagrams/01-cluster-membership.svg)

**2. A bucket's config resolves a `PlacementPolicy`.** The live peer list
from step 1, plus the bucket's own `packer_name`/`overhead_threshold_pct`,
feed `ComputedPlacement`'s rendezvous hash to pick the `k+m` peers for this
write.

![Per-bucket placement configuration](diagrams/02-placement-policy.svg)

**3. `packer_name` selects one `StripePacker` registry entry.** A bucket
only ever names a `packer_name`. Swapping in a new packer (native or,
once built, WASM) never changes `coordinator.rs` or any client-visible
behavior, only which registry entry a bucket's config points at.

![StripePacker registry](diagrams/03-packer-registry.svg)

### Write path

Placement is resolved **fresh** on every write and then pinned. This is
what makes "a node joining later never moves existing data" actually true.

```mermaid
sequenceDiagram
    participant C as Client
    participant Co as Coordinator (any node)
    participant BC as BucketConfigStore
    participant SP as StripePacker
    participant PP as PlacementPolicy
    participant EC as ErasureCoder
    participant P1 as Peer 1
    participant P2 as Peer 2
    participant PN as Peer k+m
    participant LS as LocationStore / ContentLocationStore

    C->>Co: PUT bucket/key (+ x-warpd-computable-units?)
    Co->>BC: look up bucket's packer_name + threshold
    alt content-dependent placement configured and under threshold
        Co->>SP: pack(k, units) -> stripes
        SP-->>Co: stripes (checksum-verified: reconstructs original bytes exactly)
    else plain, or packer over its overhead budget
        Co->>Co: whole object is one stripe (safe fallback)
    end
    Co->>PP: resolve k+m peers (rendezvous hash, current peer list)
    Co->>EC: encode k data shards -> m parity shards
    par concurrent fan-out, pooled gRPC
        Co->>P1: write shard 1
        Co->>P2: write shard 2
        Co->>PN: write shard k+m
    end
    P1-->>Co: ack
    P2-->>Co: ack
    PN-->>Co: ack
    Note over Co,PN: write quorum: k acks required (k+1 if m == k)
    Co->>LS: pin resolved peer set + stripe layout<br/>(replicated to every known peer, not just P1..PN)
    Co->>LS: clear any stale pin for this key in the<br/>*other* store (plain vs. packed), if one exists
    Co-->>C: 200 OK (x-warpd-placement, x-warpd-pack-overhead-pct)
```

### Read path

GET and DELETE **never** call `PlacementPolicy`. They read the pin that the
write path already recorded, regardless of what the peer list looks like
now.

```mermaid
sequenceDiagram
    participant C as Client
    participant Co as Coordinator (any node)
    participant LS as LocationStore / ContentLocationStore
    participant P1 as Peer (data shard)
    participant P2 as Peer (data shard)
    participant P3 as Peer (parity shard)
    participant EC as ErasureCoder
    participant PD as ColumnCodec (pushdown)

    C->>Co: GET bucket/key (optional Range)
    Co->>LS: look up pinned peer set / stripe layout
    Note over Co,LS: no PlacementPolicy recompute<br/>this is what keeps already-placed data stable
    alt packed object
        Co->>Co: narrow to only the stripe(s) whose<br/>units overlap the requested range
    else plain object
        Co->>Co: no sub-object structure to narrow:<br/>always the one whole-object stripe
    end
    par every one of that stripe's k+m shards, queried concurrently
        Co->>P1: fetch shard
        Co->>P2: fetch shard
        Co->>P3: fetch shard
    end
    Note over Co,P3: decode needs only k of the k+m responses<br/>(reconstructs from whichever arrive, not a staged<br/>"data first, parity if one is slow" fetch)
    Co->>EC: decode with *this object's own* stored k/m<br/>(not the node's own process-wide config)
    Note over Co,P3: read quorum: k shards available
    opt pushdown query (POST /cluster/{bucket}/{key}/query)
        Co->>PD: filter one column in place on its own peer, no reassembly
    end
    Co-->>C: 200/206 + bytes
```

**What each piece is, concretely (not aspirational, all shipped and tested
unless marked "planned"):**

- **`PlacementPolicy`** (`placement.rs`): `ComputedPlacement`, a CRUSH-style
  deterministic hash of `(bucket, key)` to a `k+m` peer set. Resolved fresh at
  PUT time and pinned into `LocationStore`. GET/DELETE read the pin, so a
  node joining or leaving never moves already-placed data.
- **`StripePacker`** (`packing.rs`): pluggable **content-dependent
  placement**, the opt-in layer on top of the always-on default above.
  `FacPacker` (Fusion's Algorithm 1, size-based bin-packing) and
  `IvfCentroidPacker` (groups by k-means cluster id, then bin-packs
  within each cluster) are both registered implementations of the same
  trait. A bucket picks one via `BucketConfigStore`, or uses neither and
  gets plain erasure-coded storage. Every packer's output is checked the
  same generic, packer-agnostic way before any shard is written: the
  stripes are reconstructed in memory and compared byte-for-byte against
  the original object, catching a dropped/duplicated/misplaced unit
  regardless of which algorithm produced it (not a bespoke check of any
  one packer's own logic). **Planned, not yet shipped:**
  `WasmPacker`: a user-submitted packer compiled to WASM, run sandboxed
  inside the coordinator process (no syscalls/network/filesystem access,
  resource-limited), simulated against a checksum of the original bytes
  for correctness before a bucket is ever allowed to go live on it. Same
  trait, same registry, same call site. Only the authoring path changes.
- **`ErasureCoder`** (`ec.rs`): `ReedSolomonCoder`, the `reed-solomon-erasure` crate
  encode/decode, with a second `encode_shards`/`decode_shards` path for
  pre-packed (content-dependent) stripes.
- **`LocationStore` / `ContentLocationStore`** (Bitcask-style: append-only
  log + in-memory index): the plain object → peer-set pin, and the
  multi-stripe unit → (offset, stripe, peers) record content-dependent
  placement needs, kept as two stores since the record shapes genuinely
  differ.
- **`cluster/s3_surface.rs`**: a minimal S3 surface (GET/PUT/HEAD/List)
  over the same coordinator path, built specifically so third-party clients
  that need bucket/list semantics (Lance's `object_store::aws`) work
  unmodified, not just protocol-agnostic range-GET clients (DuckDB's
  `httpfs`, which needs no extra surface). Deliberately minimal, stated plainly: no
  SigV4 verification (any `Authorization` header is accepted unchecked),
  no multipart upload, no LIST pagination, and `ETag`/`LastModified` in
  LIST responses are fixed placeholder values, not real ones. This is a
  distinct, much smaller surface than the single-node `/s3/` API's real
  SigV4-authenticated implementation, not the same compatibility level
  under a different URL prefix.
- **`pushdown.rs`**: a `ColumnCodec` trait + a `/query` endpoint that filters
  a column in-place on the peer holding it, no whole-object reassembly.
- **Membership**: additive only (SeaweedFS/MinIO-pool style). A new node
  calls `/cluster/join`, is added to the live peer list, and starts taking
  new writes immediately. Already-placed objects are never rebalanced
  (documented non-goal, not an oversight, see
  `docs/Distributed-Engine-Plan.md`).

Measured results for content-dependent placement, run against this engine
(Lance IVF_PQ vector search, the SIFT1M-small benchmark data, official
TPC-H via DuckDB's `dbgen` and query set) are in
[`docs/benchmarks/v1.0.0-results.md`](benchmarks/v1.0.0-results.md).

