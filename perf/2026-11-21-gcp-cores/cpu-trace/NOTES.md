# CPU trace: where node0's CPU goes per 4 KB request

Setup: #226 at b62e763, RS(2,1) on 3 nodes, node0 (n2-standard-4) pinned to 2 cores, 128 clients, 150 s PUT then 150 s GET.
In each phase, at steady load: `perf record -F 99 --call-graph dwarf` for 15 s on node0 and node1, then `perf stat` and `pidstat`
for 10 s on node0, then `strace -c` for 5 s. Every count is divided by the requests completed in the same window (`window_ops.csv`).
The VMs expose no hardware counters, so there are no cycles or IPC figures. CPU time comes from `task-clock`.

## Per request on node0

| | PUT | GET |
|---|---|---|
| CPU per request | 966 µs | 619 µs |
| context switches per request | 30 | 14 |
| futex calls per request | 16 | 10 |
| user / kernel CPU (pidstat) | 62% / 38% | 67% / 33% |
| run-queue wait (pidstat %wait) | 73% | 78% |

`cpu_per_request.png` and `cpu_per_request.csv` split the CPU per request into themes:

| Theme | PUT µs | GET µs |
|---|---|---|
| Talking to other nodes (gRPC + kernel TCP) | 253 | 190 |
| Thread hand-offs (scheduler, futex, runtime, locks) | 241 | 127 |
| malloc, free, copies | 132 | 111 |
| S3 front end (HTTP parsing + routing) | 96 | 97 |
| Disk (ext4, NVMe, fdatasync) | 53 | 6 |
| WarpDrive logic, JSON, erasure coding | 57 | 25 |
| Hashing (SigV4, SHA-256, MD5, checksum) | 39 | 13 |
| Other (tracing, clock, page faults, misc) | 96 | 50 |

## What it says

- **The work a request exists for is small.** Disk, hashing, erasure coding, JSON and WarpDrive's own logic add up to about 150 µs per PUT and 45 µs per GET.
  The other 85-90% is moving the request between threads and between nodes.
- **Talking to other nodes is the largest cost.** gRPC (tonic, h2, hpack, prost) plus the kernel's TCP path costs 253 µs per PUT and 190 µs per GET on the coordinator.
  A 4 KB object sent as shards to 2 peers costs more in RPC framing than in data.
- **Thread hand-offs are the second.** 30 context switches and 16 futex calls per PUT. The deepest WarpDrive frames on those stacks are
  `cluster::bridge::run`, `cluster::replicated_metadata::ClusterWriter` and `storage::group_commit::GroupCommit`: the request goes from
  the actix worker to the blocking pool, to the bridge runtime, to the group-commit thread and back. pidstat shows threads waiting for a core 73-78% of the time, so there are far more runnable threads than cores.
- **malloc, free and copies cost 110-130 µs** per request (glibc malloc). Buffers are cloned at each layer (`bytes::shared_clone`, `String::clone`).
- **The HTTP front end costs about 100 µs.** For GET, 58 µs is actix's router: it matches routes with regexes, one after another, on every request.
- **Disk is cheap.** fdatasync runs 1.6 times per PUT on node0 (group commit), and its CPU cost is about 50 µs.

## Syscalls per request (strace window, node0)

PUT: futex 15.6, write 4.2, recvfrom 3.7, writev 3.6, epoll_wait 1.8, fdatasync 1.6, sendto 1.6, statx 1.2, sched_yield 0.8, pwrite64 0.5.
GET: futex 9.6, recvfrom 2.5, writev 2.4, write 2.0, epoll_wait 1.9, statx 1.1, sendto 1.0, pread64 0.5.
`statx` runs once per request in both phases, a file metadata lookup that a cached handle would avoid.

## Files

- `flame-<phase>-<node>.svg`: flame graphs, Rust symbols demangled. Open in a browser to zoom and search.
- `<phase>-node<N>-categories.csv`: share of CPU per category on each node. `breakdown.txt` adds the deepest WarpDrive frame per sample.
- `<phase>-perfstat.txt`, `<phase>-pidstat.txt`, `<phase>-strace.txt`: raw counters.
- Scripts: `../scripts/cpuprof.sh` (capture), `cpu_breakdown.py` (categories), `cpu_per_request.py` (this table and plot).
