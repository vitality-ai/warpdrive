# Async metadata write A/B

Question: if the PUT handler awaits the metadata row write instead of blocking its HTTP worker on the bridge pool,
do context switches and CPU per request fall?

One binary: b62e763 plus `WARPDRIVE_ASYNC_META` (branch `feat-async-metadata-path`, 78fcc1a, off by default).
With the switch on, the metadata write runs as a task on the bridge runtime and the handler awaits it. Per-key locks are
async-aware mutexes in both arms. RS(2,1), node0 pinned to 2 or 4 cores, 128 clients, 120 s PUT then 120 s GET,
arms alternated, 2 runs each. Per-pool counters from `/proc` at phase boundaries. `summary.csv`.

| node0 | PUT/s sync -> async | GET/s | CPU per PUT | switches per PUT |
|---|---|---|---|---|
| 2 cores | 2,091 -> 1,997 (-4.5%) | 3,448 -> 3,440 | 880 -> 942 µs | 30.1 -> 28.6 |
| 4 cores | 2,995 -> 2,911 (-2.8%) | 4,950 -> 4,781 | 1,140 -> 1,144 µs | 31.8 -> 27.6 |

Per pool, PUT, 2 cores:

| pool | sync | async |
|---|---|---|
| bridge pool (wd-bridge) | 9.3 switches, 205 µs | 5.5 switches, 111 µs |
| bridge runtime (wd-bridge-rt) | 7.3 switches, 273 µs | 7.7 switches, 386 µs |
| HTTP workers (actix-rt) | 2.2 switches, 222 µs | 3.3 switches, 245 µs |

No errors, no panics.

## What it says

The bridge pool's share fell by half, and the bridge runtime and HTTP workers picked it up and slightly more. Blocking is
not the cost. Crossing runtimes is: a PUT still goes from actix's per-worker runtime to the bridge runtime, to the
HTTP and gRPC connection tasks, and back, and each crossing is a wakeup on another thread.

Together with the bridge-direct A/B (`../handoff-ab/`), two changes that removed one hop each did not lower CPU per request.
The cost belongs to the cross-runtime design as a whole. The change expected to lower it keeps a request on the thread it
arrived on: peer connections per HTTP worker, on that worker's runtime, with no shared bridge runtime.
