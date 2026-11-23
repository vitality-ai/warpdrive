# Advanced Setup

Most deployments need nothing from this page: the defaults are chosen so a node works well on any machine. Read on if you
want to know what a node keeps in memory, how to size machines, or when to change the one tuning setting.

Every number here was measured on GCP. The raw data, the drivers and a consolidated
[report](https://github.com/vitality-ai/warpdrive/blob/feat-benchmark-results/perf/REPORT.md) are on the
`feat-benchmark-results` branch (`perf/2026-11-22-gcp`: `metadata-engines`, `sizing-bitcask`, `sizing-rocksdb`,
`sizing-combined`, and `perf/2026-11-23-gcp-rs32`: `size-sweep`, `soak`). Unless noted, the cluster was 3 nodes with
RS(2,1), local NVMe, and 64 clients writing 4 KB objects.

## The metadata engine

Each node keeps the cluster's object metadata (which objects exist, their versions, tags and where their bytes live) and
its own shard index (where each shard it holds sits on its disk). Both live in the metadata engine, chosen with
`WARPDRIVE_META_ENGINE`:

| Engine | Where rows live | Memory | When to use it |
|---|---|---|---|
| `rocksdb` (default) | on disk, hot blocks cached | bounded by the block cache | always, unless you have a reason below |
| `bitcask` | every row in RAM, plus an append-only log | grows with every object, on every node | small, fixed data sets that must have the fastest reads |
| `sqlite` | one SQLite table | bounded by SQLite's page cache | testing |

The engine can't be changed on a node that has data: there's no migration between engines. A node set to one engine
refuses to start next to another engine's metadata, and says which setting to use, so it never comes up empty by mistake.

### RocksDB against Bitcask

Engine alone, 1 M rows of 816 bytes, 64 writers, every write durable:

| | Bitcask | RocksDB, 256 MB cache | RocksDB, 1 GB cache |
|---|---|---|---|
| Durable writes/s | 5,160 | 21,600 | 21,300 |
| Memory after 1 M rows | 1,557 MB, growing | 162 MB | about 900 MB once the cache fills |
| Reads/s (p99) | 876,000 (3 us) | 163,000 (10 ms) | 247,000 (0.7 ms) |
| Restart | 4.4 s | 0.2 s | 0.2 s |
| On disk | 953 MB | 186 MB | 186 MB |

RocksDB writes faster, keeps memory flat and restarts at once. Bitcask reads faster, because every row is already in
memory, and that is also why it can't hold more objects than fit in RAM.

In a full cluster the difference is a fixed cost per request, so it shows on small objects and vanishes on large ones.
On RS(3,2) (5 x 2 vCPU nodes), RocksDB against Bitcask:

| Object size | PUT | GET |
|---|---|---|
| 4 KB | -10% | -11% |
| 64 KB | -16% | -16% |
| 1 MB | -3% | -7% |
| 8 MB | -6% | +1% |
| 64 MB | 0% | 0% |

Closing the small-object gap is ongoing work.

## Memory: what a node uses

| What | Size | Setting |
|---|---|---|
| RocksDB block cache | an eighth of the machine's memory, at least 256 MB | `WARPDRIVE_ROCKSDB_CACHE_MB` |
| RocksDB write buffers | up to 64 MB for each of the two databases | fixed |
| Recently read objects | 256 MB | `WARPDRIVE_BLOB_CACHE_MB` (0 disables) |
| The server itself | a few hundred MB, more under heavy load | |

With RocksDB none of these grows with the number of objects. Idle memory per node after a restart, measured:

| Objects | Bitcask | RocksDB |
|---|---|---|
| about 1.5 M | 5.4-5.5 GB | 0.25 GB |
| about 4.3 M | out of memory past about 2 M (8 GB nodes) | 0.7-0.9 GB |

It rises slowly while RocksDB's cache fills, and stays within the cache size. Startup takes about 1 s at any of these
counts (Bitcask: 23 s at 1.7 M, because it loads every row). A node logs its cache size at startup:

```
RocksDB block cache: 1024 MB (WARPDRIVE_ROCKSDB_CACHE_MB)
```

### With Bitcask: size for every object

Bitcask keeps every object's metadata in RAM on every node, about **3.3 KB per object** (measured at 1.7 M and 2.0 M
objects). Every node holds the whole cluster's metadata, so adding nodes doesn't raise the limit:

| RAM per node | Objects the whole cluster can hold, about |
|---|---|
| 8 GB | 2 M |
| 16 GB | 4 M |
| 64 GB | 18 M |

Past the limit the kernel stops WarpDrive for running out of memory, and it can't restart on the same machine, because
startup loads every row again. Use RocksDB unless your object count is small and fixed.

## The one tuning setting: the RocksDB block cache

`WARPDRIVE_ROCKSDB_CACHE_MB` sets how much memory RocksDB uses to cache metadata. The default, an eighth of the machine's
memory, suits most deployments.

**What it changes.** Metadata that fits in the cache is read without touching disk or decompressing. On the engine
benchmark above (1 M rows, about 900 MB of metadata):

| Cache | Reads/s | Read p50 | Read p99 | Memory |
|---|---|---|---|---|
| 256 MB | 163,000 | 12 us | 10 ms | 424 MB |
| 1 GB | 247,000 | 7 us | 0.7 ms | 911 MB |

Writes don't change with the cache size.

**When to raise it.** When the metadata your clients read often is larger than the cache. The sign is GET tail latency
(p99) rising while CPU and disk aren't saturated. The cache holds metadata decompressed, about 1 KB per object, so each
GB of cache holds the metadata of about 1 M objects (on disk it's compressed to about 250 bytes per object). Size the
cache for the objects that are read often, not for every object.

**What it costs.** Exactly that much memory, and no more. Leave room for the operating system's page cache, which
RocksDB also benefits from.

Settings we measured that don't help reads, and so aren't exposed: a row cache, a hash index inside data blocks, and
turning compression off.

## Disk headroom while overwriting

Space from overwritten and deleted objects comes back after a grace period (`WARPDRIVE_RECLAIM_GRACE_SECS`, default
600 s), which protects reads still in flight. While objects are overwritten steadily, keep free disk of about:

```
write rate x (grace period + 1 to 2 minutes)
```

For example, 100 MB/s of overwrites with the default grace needs about 70 GB free.

## Open files

Object data lives in 64 MB segment files. A node keeps at most 4,096 of them open and raises its own open-file limit at
startup. The `warpdrive.service` unit sets `LimitNOFILE=1048576`. If you run WarpDrive another way, give it a high
open-file limit too (`ulimit -n`).
