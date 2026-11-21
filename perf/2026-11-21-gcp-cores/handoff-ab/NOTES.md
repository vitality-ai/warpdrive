# Hand-off A/B and context switches per thread pool

Question: are thread hand-offs the cause of the high CPU per request (see `../cpu-trace/`)?

## A/B: skip the bridge-thread hop

One binary: b62e763 plus `WARPDRIVE_BRIDGE_DIRECT` (branch `feat-bridge-direct-experiment`, f1d9562, off by default).
With the switch on, a blob read or write that arrives on a blocking-pool thread drives its future there instead of handing it
to a bridge thread. The metadata write runs on the actix HTTP worker (an async context), so it keeps the bridge pool.
RS(2,1), node0 pinned to 2 or 4 cores, 128 clients, 120 s PUT then 120 s GET, arms alternated, 2 runs each. `summary.csv`.

| node0 | PUT/s pool -> direct | GET/s pool -> direct | switches per PUT | CPU per PUT |
|---|---|---|---|---|
| 2 cores | 2,121 -> 2,121 | 3,410 -> 3,484 | 30.4 -> 29.4 | 911 -> 908 µs |
| 4 cores | 3,072 -> 3,046 | 5,069 -> 4,982 | 33.2 -> 32.0 | 1,119 -> 1,119 µs |

No measurable effect. Differences are inside the run-to-run spread (about 8%). This hop is about 1 of the 30 switches per PUT.

Also in this data: going from 2 to 4 cores raises CPU per request by 23% for PUT and 29% for GET. The same work costs more
CPU on more cores, the sign of contention or cross-core wakeups.

## Context switches per thread pool (node0, 2 cores, baseline)

Per-thread `/proc` counters over 30 s of steady load, divided by the requests in the same window (`threads/`).

| pool (PUT, node0) | threads | switches per PUT | CPU per PUT |
|---|---|---|---|
| tokio-rt-worker (blocking pools, gRPC server) | 199 | 10.8 | 120 µs |
| wd-bridge (bridge pool) | 64 | 9.5 | 201 µs |
| wd-bridge-rt (bridge runtime, gRPC client) | 54 | 7.5 | 277 µs |
| actix-rt (HTTP workers) | 32 | 2.5 | 219 µs |

About 350 threads share 2 cores. A request crosses four pools. GET is lighter (14 switches) with the same shape.
node1 (shard holder) does 19 switches per PUT, mostly tokio-rt-worker (gRPC server plus spawn_blocking to the store).

## Next experiments (one factor each)

1. Size the pools to the cores: `WARPDRIVE_BRIDGE_THREADS` (exists, default 64), actix blocking pool, tokio blocking threads.
2. Move the metadata write off the HTTP worker, or make the metadata path async end to end, so the bridge pool drops out.
3. Contention fit at 1, 2, 3, 4 cores to pin down how CPU per request grows with cores.
