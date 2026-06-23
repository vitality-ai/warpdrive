# RFC 3.1: Block Device Storage

**Status:** Design  
**Date:** June 2026  
**Parent:** [RFC 3 — Segment Storage](RFC3-medium.md)

---

## Background

RFC3 identified the core problem with Warpdrive's original storage layer: one flat append-only file per bucket that grows forever with no way to reclaim space from deleted objects. RFC3's answer was segment files — split the bucket file into 512 MB chunks, track utilization via SQLite, compact cold segments by copying survivors and deleting the whole segment file.

That design is sound. But in the process of reasoning about RFC3, three further problems surfaced that segments alone do not fully solve. This document records that reasoning and where it led.

---

## The Three Problems That Remained

### 1. Space reclamation requires a background compaction process

RFC3's compaction is correct and crash-safe, but it is a background process with meaningful complexity: a cold threshold T, a GC worker, survivor copy, atomic SQLite commit, grace-period deletion. T must be tuned. Compaction must not interfere with active writes (the Option A vs B analysis). The system has a transient 2× space amplification window during compaction.

The question is whether space reclamation requires any of this machinery, or whether there is a simpler primitive.

### 2. Segment files multiply inode count

The current flat-file design gives one inode per bucket — that inode stays permanently hot in the OS page cache. RFC3 segments replace this with one inode per 512 MB of data written. For a 10 TB bucket, that is ~20,000 segment files and ~20,000 inodes.

This connects directly to the Haystack paper's core observation. Facebook's original NFS-backed photo storage required three disk operations per photo read: directory entry → inode → data. The inode was the bottleneck — with millions of photos as individual files, the inode table could not fit in memory. Haystack's solution was to pack millions of photos into a small number of large pre-allocated files ("physical volumes"), each with a single inode that stayed permanently cached. XFS was chosen specifically because it supports efficient file pre-allocation and because the blockmap for a single large contiguous file — just one extent — is trivially small and always in cache.

RFC3 segments partially recover this property at large segment sizes, but the inode count still scales with data volume. With hundreds of thousands of segment files across all buckets, inode cache pressure becomes a real concern.

### 3. Filesystem hole punching does not compose with append-only writes

A natural response to the compaction complexity is hole punching: on delete, call `fallocate(FALLOC_FL_PUNCH_HOLE, offset, size)` to return those physical blocks to the OS immediately, with no background process. The offset stays valid (reads of surviving objects are unaffected), the disk reports lower usage, no compaction needed.

This breaks down in an append-only model. The write pointer only moves forward — new writes go to EOF, never back into the holes. Freed blocks are returned to the filesystem's free pool, but on a dedicated Warpdrive storage volume with no other tenants, nothing writes into them. The holes accumulate as freed-but-never-reused extents. The XFS extent tree for the file grows with every punch, increasing blockmap complexity. When the disk eventually fills and XFS starts placing new allocations into hole extents, sequential write behavior degrades.

The freed space is real (df reports it correctly), but it does not benefit Warpdrive's own write path. The append-only invariant and hole punching are structurally incompatible.

### 4. Slab allocation within a single file — and why it also fails

The hole punching failure reveals the root problem: **variable-size objects produce variable-size holes, and variable-size holes cannot be reliably reused**. This is the classic external fragmentation problem from memory allocators.

The slab allocator's answer to external fragmentation: fixed-size slots. Applied to file storage: pre-allocate one large file per bucket (preserving the Haystack single-inode benefit), divide it internally into fixed-size slots — say 4 MB each. Objects under 4 MB take one slot; larger objects span consecutive slots. On delete, mark the slot free in a SQLite free list. On write, check the free list first; if a slot is available, write there instead of extending EOF.

Now freed space IS reusable — because every hole is exactly slot-sized, and every new object under 4 MB fits in exactly one slot. No external fragmentation. No compaction worker. No hole-size mismatch.

The tradeoff: internal fragmentation. A 1 KB object wastes most of a 4 MB slot. Multiple size classes (like jemalloc's size bins) bound this — at the cost of maintaining several free lists and choosing a bin at allocation time.

**Why this still falls short**: the filesystem is still in the critical path. Every slot read and write goes through XFS's inode, blockmap, and journal. The slab design solves the space management problem but does not address the filesystem overhead that Ceph BlueStore's results quantify: 15% average latency reduction, 80% tail latency reduction from simply removing XFS from the data path. Additionally, implementing a slot allocator inside a filesystem file means managing two allocators simultaneously — XFS's own free space manager and our slot free list — which interact in subtle ways when the filesystem decides to defragment or reallocate extents.

The slab idea is correct in structure but wrong in substrate. The right substrate is one where we own the allocation entirely — a raw block device.

---

## Literature Survey

### Haystack — Facebook, OSDI 2010

_Finding a Needle in Haystack: Facebook's Photo Storage_  
Beaver et al., USENIX OSDI 2010

The inode problem at Facebook's scale: storing hundreds of billions of photos as individual files meant the inode table was measured in terabytes and could never fit in memory. Each read required three disk seeks. Haystack's solution: pack photos into a small number of large pre-allocated files (physical volumes) on a single 10 TB XFS filesystem per storage machine. One inode per volume, permanently hot. An in-memory index maps photo ID to `(volume_id, offset, size, flags)` loaded from a compact index file at startup. Reads then require one disk seek.

XFS was chosen for two specific properties: (1) efficient `fallocate`-style pre-allocation — reserving a contiguous extent at volume creation time so the blockmap is a single entry, trivially small, never evicted from cache; (2) large file blockmaps fit entirely in memory.

Space reclamation: copy survivors to a new volume, delete the old one. Compaction is whole-volume and infrequent. The design accepts that compaction is expensive precisely because it is rare.

**Key lesson for Warpdrive**: the inode and blockmap are filesystem-level metadata that become the bottleneck when files are many and small. The answer is fewer, larger files — not better inode caching.

---

### Bitcask — Riak, 2010

_Bitcask: A Log-Structured Hash Table for Fast Key/Value Data_  
Sheehy and Smith, Basho Technologies

An append-only key/value store where all writes go to the tail of an active log file. An in-memory `keydir` hash table maps every key to its latest value location: `(file_id, offset, size, timestamp)`. Reads are always one disk seek. Old versions of overwritten keys remain on disk until a merge process produces new compacted files containing only the latest live version of each key.

Bitcask is the direct ancestor of Warpdrive's current metadata model: SQLite's `objects` table is the durable equivalent of the in-memory `keydir`, storing `(file_id, offset, size)` per object. The data file is Warpdrive's blob storage file.

**Key lesson for Warpdrive**: Bitcask's merge is unavoidable in an append-only log model. The only way to avoid merge/compaction is to not use a pure append-only log for the data plane — i.e., to use a model that allows space reuse in-place.

---

### WiscKey — USENIX FAST 2016

_WiscKey: Separating Keys from Values in SSD-conscious Storage_  
Lu, Pillai, Arpaci-Dusseau, Arpaci-Dusseau, University of Wisconsin

WiscKey's central insight: LSM-tree compaction amplifies writes because it rewrites both keys and values together. On SSDs, random reads are cheap, so keys and values can be separated. Keys live in the LSM tree (small, sorted, compaction is cheap). Values live in a separate append-only value log (vlog). Compaction only rewrites keys — the value log is never reorganized. Space reclamation from the vlog is a separate, cheaper GC process.

The split is directly analogous to Warpdrive's architecture: SQLite (keys/metadata) is already separated from the blob data (values). WiscKey validates that this separation is the right structural choice and that the two planes can have independent reclamation strategies.

**Key lesson for Warpdrive**: the metadata plane and the data plane have different access patterns and different reclamation needs. Designing them separately is correct. The data plane's reclamation does not need to involve the metadata plane's compaction.

---

### Ceph BlueStore — 2017

_Understanding Write Behaviors of Storage Backends in Ceph Object Store_  
MSST 2017; BlueStore production deployment

Ceph's original FileStore backend wrote object data as files on an XFS filesystem. Every write was journaled twice: once to the Ceph journal (for crash safety) and once to the XFS journal (inherent to the filesystem). This "double write penalty" tripled actual write traffic for every user byte.

BlueStore eliminated this by bypassing the filesystem entirely. Object data is written directly to the raw block device. Metadata (object name → block location) is stored in RocksDB, running on a separate SSD or partition. Space management uses a custom buddy allocator with a minimum allocation unit of 4 KB. There is no filesystem journal for data writes — crash safety comes from RocksDB's WAL for metadata and careful ordering of data writes before metadata commits.

Results: 4 KB random write IOPS improved 18%, average latency decreased 15%, 99.99th percentile tail latency decreased up to 80% compared to FileStore/XFS.

**Key lesson for Warpdrive**: the filesystem is an abstraction that costs performance. For a storage system that manages its own metadata, the filesystem provides nothing that cannot be replaced more efficiently. Bypassing it is not exotic — it is the production decision Ceph made.

---

### blobd — Wilson Lin, 2024

_Building blobd: single-machine object store with sub-millisecond reads and 15 GB/s uploads_  
https://blog.wilsonl.in/blobd/

A single-machine object store built directly on raw block devices with O_DIRECT, using a buddy allocator (power-of-2 sizes from 4 KB to 16 MB) for space management and an in-memory hash map for O(1) object lookup. No filesystem involved for data.

Benchmark results (8× 3.84 TB NVMe SSDs):

| System | 12 KB read latency | 31 MB read latency |
|--------|-------------------|-------------------|
| blobd  | 0.33 ms           | 0.77 ms           |
| XFS    | 22.5 ms           | 20.8 ms           |
| MinIO  | 66.4 ms           | 100.2 ms          |

blobd outperforms XFS by 68× for small objects. The gains come from eliminating inode lookups, page cache copies, and filesystem metadata overhead entirely.

**Key lesson for Warpdrive**: for object storage, bypassing the filesystem is achievable by a single engineer, delivers order-of-magnitude improvements, and the buddy allocator solves space management simply and correctly.

---

### io_uring — Linux 5.1+, 2019

_Efficient IO with io_uring_, Axboe (Linux kernel)

io_uring is a Linux async I/O interface that submits and completes I/O operations through shared ring buffers, avoiding the per-syscall overhead of `pread`/`pwrite`. Key capabilities relevant to Warpdrive:

- **O_DIRECT integration**: pairs naturally with block device access; bypasses page cache
- **Registered buffers**: user-space DMA buffers pinned once at startup; kernel DMA writes directly into them — ~11% throughput improvement
- **Completion polling (IOPOLL)**: available for O_DIRECT block device access; avoids interrupt overhead — ~21% additional throughput gain
- **NVMe passthrough**: `IORING_OP_URING_CMD` issues native NVMe commands, bypassing the generic storage stack for another ~20% gain

io_uring is not required for the block device design to work — `pread`/`pwrite` with O_DIRECT is sufficient — but it is the natural next step once the filesystem is out of the critical path.

---

## What the Industry Does

The five systems below span the largest object storage deployments in production. Taken together they show a clear trajectory: every system that began on a local filesystem eventually built something to work around it, and the systems built after 2015 either bypass it entirely or treat it as a thin wrapper they control completely.

---

### Facebook Haystack + f4 — XFS with pre-allocation

**Haystack** (OSDI 2010) stores photos in a small number of large pre-allocated files on a single 10 TB XFS filesystem per storage machine. One inode per physical volume, permanently cached. In-memory index maps photo ID to `(volume_id, offset, size)`. Space reclaim by whole-volume compaction.

**f4** (OSDI 2014) is Haystack's successor for warm (less frequently accessed) data. The node-level layout is identical: a data file, an index file, and a new journal file that tracks deletes for locked (sealed) volumes. f4 does not change the storage node design — its contribution is replacing Haystack's 3.6× replication factor with Reed-Solomon erasure coding (10 data + 4 parity blocks), dropping storage overhead to 2.1×.

**Key observation**: Facebook stayed on XFS for over a decade. Their workaround for the filesystem's inode overhead was not to replace the filesystem but to use it as little as possible — one file per volume, pre-allocated, with all metadata in-memory. The filesystem is present but nearly invisible.

---

### Microsoft Azure Blob Storage (WAS) — local filesystem, extent files

**WAS** (SOSP 2011) uses a three-layer architecture: Front-End, Partition Layer, Stream Layer. The Stream Layer is the relevant one for storage node internals. It stores data as **extents** — append-only units of ~1 GB, each stored as a file on the local filesystem of an extent node. Extents are composed of variable-length **blocks** (up to 4 MB), which are the unit of client read/write. Each extent node maintains a block index mapping byte offsets to block locations within the extent file.

Like Haystack, WAS uses the local filesystem but keeps the file count small (one file per extent) and keeps metadata (the block index) local to the extent node rather than in the filesystem. Sealed extents are erasure-coded to reduce storage overhead.

**Key observation**: WAS explicitly chose to keep extents as files on a local filesystem, not raw block devices. The filesystem is used as a durable append-only file store with all higher-level structure (blocks, indexes, replication) managed by the WAS stream layer itself. The inode problem is sidestepped by making extents large (1 GB) and infrequent.

---

### Google Colossus — distributed chunk servers, BigTable metadata

**Colossus** (GFS successor, 2010) replaced GFS's single-master architecture with distributed metadata stored in BigTable. D file servers act as network-attached storage nodes — effectively raw storage managed by Colossus. BigTable stores the file → chunk location mapping. Storage efficiency improved from GFS's 3× replication to ~1.5× via erasure coding.

Colossus does not publish node-level storage details. D servers are described as "network-attached disks with minimal data hop paths." The implication is that the filesystem abstraction is largely invisible — Colossus manages chunk placement and the D servers provide block-level access. The metadata layer (BigTable) is fully separated from the data layer (D servers), a clean control-plane / data-plane split.

**Key observation**: Google separated metadata management (BigTable, queryable, distributed) from data storage (D servers, dumb, append) in 2010. This is structurally identical to RFC3.1's SQLite-for-metadata + block-device-for-data split.

---

### Ceph BlueStore — raw block device, RocksDB metadata

**BlueStore** (production 2017, default since Ceph Luminous) is the clearest industry validation of the block-device approach. The motivation was the "double write penalty" of FileStore: every write was journaled once by Ceph's WAL and again by XFS's journal — tripling actual write traffic per user byte.

BlueStore eliminates this by bypassing the filesystem entirely. Object data is written directly to the raw block device. All metadata (object name → block location on device) lives in RocksDB on a separate SSD. Space management uses a buddy allocator with a 4 KB minimum allocation unit. There is no filesystem journal for data — crash safety comes from write ordering (data before metadata commit) and RocksDB's own WAL for metadata.

Measured results vs FileStore/XFS:
- 4 KB random write IOPS: +18%
- Average latency: −15%
- 99.99th percentile tail latency: −80%

**Key observation**: Ceph is the largest open-source object storage system in production. Their explicit conclusion after running both FileStore and BlueStore at scale: the filesystem is a bottleneck that adds journaling overhead, double-buffering, and semantic mismatch. Removing it produced the largest single performance improvement in Ceph's history.

---

### MinIO — local filesystem (XFS), erasure coding across drives

**MinIO** represents the opposite end of the spectrum: it uses the local filesystem directly, storing each object as a regular file (with an `xl.meta` sidecar for metadata) on XFS. Reed-Solomon erasure coding is applied across drives within a node and across nodes. Bitrot protection via per-shard checksums. No raw block device access.

MinIO's design is simple and portable — it runs on any POSIX filesystem. The tradeoff is that it inherits all the filesystem's overhead: per-object inode, blockmap, filesystem journal, page cache double-buffering. This is precisely why blobd benchmarks 68× lower latency than XFS for 12 KB objects — MinIO's architecture, at the node level, is closer to what XFS gives you out of the box.

**Key observation**: MinIO proves that filesystem-based object storage is viable and operationally simple. It is not the highest-performance option. It is the fastest-to-build option — which is a valid trade-off at certain scales.

---

### Backblaze B2 — ext4 per drive, erasure coding below the filesystem

Backblaze is relevant not because they do something technically advanced at the node level, but because they made a deliberate, documented choice to use **ext4 on every drive** — and they explain exactly why.

A Backblaze Vault is 20 storage pods × 60 drives = 1,200 drives. Each drive runs a standard ext4 filesystem. Their custom erasure coding (Reed-Solomon, 17 data + 3 parity shards) operates *below* the S3 abstraction but *above* each individual drive's filesystem. A file is split into 20 shards across 20 pods, one shard per pod, written as a regular file on that pod's ext4 drive.

The key design rationale: by placing erasure coding *below* the filesystem rather than above it, a corrupted filesystem can lose at most one shard of any file — not the whole file. Each drive is an independent failure domain at the filesystem level.

Backblaze does not bypass the filesystem and does not pursue sub-millisecond latency. Their goal is the lowest cost per GB in the market. ext4 is the right tool for that: simple, well-understood, zero custom code. Their innovation is in hardware pod design and erasure coding layout, not in the storage node's I/O path.

**Key observation**: Backblaze is the existence proof that filesystem-based storage is viable and profitable at exabyte scale — if cost, not latency, is the primary constraint. The filesystem overhead is real but acceptable when your drives are HDDs spinning at 7,200 RPM anyway.

---

### CoreWeave + VAST Data — NVMe-over-Fabrics, SCM metadata, GPU-local cache

CoreWeave's AI Object Storage (GA March 2025) is the most architecturally modern system in this survey. The backend object repository is **VAST Data** (via a $1.17B deal announced November 2025). The caching layer is CoreWeave's own **LOTA** (Local Object Transport Accelerator).

**VAST Data's architecture** at the node level:

- Completely bypasses the filesystem. Data is written as raw chunks to NVMe flash, addressing full erase blocks directly (flash-aware, not POSIX-aware).
- Every compute node (CNode) has direct NVMe-over-Fabrics access to every SSD in the cluster — truly shared-everything, no data partitioning.
- Metadata lives on Storage Class Memory (SCM, e.g., Optane), not on flash. SCM also acts as a write buffer — data reduction (compression, deduplication, similarity encoding) happens during background migration from SCM to flash, not inline. Write latency is therefore unaffected by how expensive the data reduction is.
- Erasure coding uses ultra-wide stripes (up to 146 data + 4 parity = 150 strips) with locally decodable codes — reconstruction requires reading only 1/4 of the surviving strips, not all of them.
- **Similarity reduction**: beyond standard deduplication, VAST identifies data chunks that are *similar but not identical* and compresses the delta between them. This is meaningfully different from anything the other systems in this survey do.

**LOTA** sits on each GPU node as a caching proxy. When an object is first accessed, LOTA caches it on the GPU node's local NVMe (1 TB per node in CoreWeave's benchmark cluster). Subsequent reads hit local NVMe, not the network. Aggregate cache-warmed throughput: 368 GiB/s across 20 nodes = 2.3 GiB/s per GPU. Performance is entirely driven by local NVMe once the cache is warm — the backend (VAST) is only on the critical path for cache misses.

**Key observation**: CoreWeave + VAST is the AI-era endpoint of the trajectory Ceph BlueStore started. The filesystem is gone. SCM replaces DRAM for metadata. NVMe-over-Fabrics collapses the distinction between local and remote storage. The GPU-node-local NVMe cache (LOTA) closes the last remaining latency gap between the storage backend and the compute that needs the data.

---

### The Pattern

| System | Node storage | Metadata | Filesystem | Optimised for |
|--------|-------------|----------|-----------|--------------|
| Haystack / f4 | Large pre-alloc files on XFS | In-memory + index file | Present, minimised | Web-scale photo serving |
| Azure WAS | Extent files on local FS | Per-node block index | Present, minimised | General cloud storage |
| Google Colossus | D file servers (opaque) | BigTable | Largely bypassed | Google-scale everything |
| Ceph BlueStore | Raw block device | RocksDB | Eliminated | General purpose, open source |
| DAOS | Bypasses VFS entirely | In-process SCM | Eliminated | HPC / NVMe-native |
| Backblaze B2 | ext4 per drive | Custom shard index | Full reliance | Cost per GB |
| MinIO | Files on XFS | xl.meta sidecar | Full reliance | Simplicity, portability |
| CoreWeave + VAST | Raw NVMe-over-Fabrics | SCM | Eliminated | AI training throughput |
| blobd | Raw block device | In-memory hash map | Eliminated | Low-latency single node |

The trajectory is unambiguous. Systems optimised for cost (Backblaze) or simplicity (MinIO) stay on the filesystem. Every system optimised for performance at scale — from Ceph in 2017 to VAST in 2025 — has eliminated or bypassed it. The metadata plane is always separated from the data plane. Warpdrive's RFC3.1 design follows the performance-optimised path.

---

## Where We Are

The literature survey and the reasoning above converge on the same place independently:

**The filesystem is the wrong abstraction for a storage system that manages its own metadata.**

Warpdrive already manages all metadata in SQLite: object locations, bucket membership, multipart state. The filesystem's inode, blockmap, and journal exist to provide exactly this — a way to locate file data and survive crashes. Warpdrive does not need the filesystem to do this. It already does it in SQLite, which is more queryable, more compact, and crash-safe via its own WAL.

What the filesystem currently provides for Warpdrive's data plane:

| Filesystem provides | Warpdrive replacement |
|--------------------|----------------------|
| Inode (file identity + location) | `objects.file_id` + block offset in SQLite |
| Blockmap (which disk blocks = file data) | Absolute byte offset stored in SQLite — no indirection needed |
| Free space tracking | `free_extents` table in SQLite |
| Journal (crash safety for data) | Write ordering: data before metadata commit |
| Directory (namespace) | `buckets` table in SQLite |

Removing the filesystem removes all of these as redundant layers. What remains is cleaner:

```
┌──────────────────────────────┐    ┌─────────────────────────────────┐
│  SQLite  (metadata)          │    │  Block device or pre-alloc file │
│  objects: offset, size       │    │  [ superblock @ block 0       ] │
│  buckets                     │    │  [ object bytes.............. ] │
│  free_extents: offset, size  │    │  [ free extent............... ] │
│  multipart_*                 │    │  [ object bytes.............. ] │
└──────────────────────────────┘    └─────────────────────────────────┘
         control plane                         data plane
         (existing, unchanged)                 (new)
```

---

## Proposed Design

### Storage target

In production: raw block device (`/dev/nvme0n1` or similar), opened with `O_RDWR | O_DIRECT`.  
In development and testing: a pre-allocated regular file, same flags. Code path is identical — both are a file descriptor with `pread`/`pwrite`.

```rust
// dev / test
let fd = OpenOptions::new().read(true).write(true)
    .custom_flags(libc::O_DIRECT | libc::O_CREAT)
    .open("warpdrive.data")?;

// production
let fd = OpenOptions::new().read(true).write(true)
    .custom_flags(libc::O_DIRECT)
    .open("/dev/nvme0n1")?;
```

### Superblock

Block 0 is reserved for the superblock: device size, format version, root of any bootstrap metadata. Small and fixed. Written once at initialization, read once at startup.

### Space management

A new SQLite table replaces the filesystem's free space manager:

```sql
CREATE TABLE free_extents (
    offset  INTEGER NOT NULL,
    size    INTEGER NOT NULL
);
CREATE INDEX free_extents_size ON free_extents(size, offset);
CREATE INDEX free_extents_offset ON free_extents(offset);
```

**Allocation** (new write):
1. `SELECT offset FROM free_extents WHERE size >= ? ORDER BY size LIMIT 1` — best-fit
2. If found: remove that row, re-insert remainder if partially used
3. If not found: advance the write pointer (next free block past all previously written data)
4. All in one SQLite transaction with the `objects` insert

**Deallocation** (delete):
1. Insert `(offset, size)` into `free_extents`
2. Coalesce with adjacent free extents (query neighbours by offset, merge if contiguous)
3. In same SQLite transaction as `objects` delete

Space is reusable immediately after delete. No background worker. No compaction. No threshold tuning.

### Alignment

O_DIRECT requires all offsets and sizes to be multiples of the device logical block size (512 B or 4096 B). All allocations are rounded up to the nearest 4096 B boundary. Maximum internal fragmentation per object: 4095 B — negligible.

### Write path

```
client PUT
  → compute aligned size
  → allocate extent (SQLite free_extents or advance write pointer)
  → pwrite(fd, data, aligned_size, offset)   [O_DIRECT, data hits device]
  → SQLite transaction: INSERT INTO objects, UPDATE free_extents
  → acknowledge to client
```

Data is written before the metadata commit. If the process crashes after the pwrite but before the SQLite commit, the `objects` row does not exist — the written bytes are invisible and the extent is not in `free_extents` (leaked space). A startup scan can detect and recover these orphaned extents if needed, but the window is small and the data is not corrupted.

### Read path

```
client GET
  → SELECT offset, size FROM objects WHERE ...   [SQLite]
  → pread(fd, buf, size, offset)                 [O_DIRECT, one seek]
  → stream to client
```

One SQL lookup. One disk seek. No inode fetch. No blockmap fetch. No filesystem layer.

### Crash safety

SQLite's WAL provides atomicity for all metadata operations. The write ordering (data before metadata) ensures that a partial crash leaves the system in a consistent state: either the object is fully committed (data and metadata both written) or the object does not exist (metadata not committed). Partial data writes with no metadata row are invisible to clients and recoverable at startup.

---

## Properties

| Property | Current (filesystem) | This design |
|----------|---------------------|-------------|
| Inode overhead | 1 per segment file | None |
| Blockmap overhead | Grows with file fragmentation | None |
| Space reclaim | RFC3 compaction (background GC) | Immediate, on delete |
| Compaction required | Yes | No |
| Write path | append → filesystem journal → data | allocate → pwrite → SQLite commit |
| Read path | SQLite → inode → blockmap → data | SQLite → pread |
| Disk seeks per read | 1–3 (inode + blockmap + data) | 1 (data only) |
| Dev/test mode | Regular file on filesystem | Regular file, same code |
| External fragmentation | Managed by XFS allocator | Managed by free_extents table |

---

## Open Questions

1. **Allocation policy**: best-fit minimises fragmentation; first-fit is faster. Benchmark both against real workload distributions before deciding.
2. **Free extent coalescing**: merge adjacent free extents on every delete (correct, small overhead) or batch coalescing in a background pass (complex, probably not needed).
3. **Startup orphan recovery**: scan `free_extents` + `objects` to detect leaked extents from crash-interrupted writes. Define the recovery procedure.
4. **io_uring**: `pread`/`pwrite` with O_DIRECT is sufficient for v1. io_uring (registered buffers, IOPOLL) is the natural next step for v2 — adds ~30% throughput, no design change needed.
5. **Multi-device**: one block device per Warpdrive node is the starting point. Striping across multiple devices (for throughput) is a later concern — the `free_extents` table naturally extends to `(device_id, offset, size)`.
6. **SQLite as bottleneck**: at very high write rates, the SQLite transaction per write (allocation + insert into objects) may become the bottleneck before the block device does. Profile before optimising.
