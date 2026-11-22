# HTTP workers per core

Question: #226 (ee6c06b) set HTTP workers to max(32, cores). Is that why its GETs trail main?

Builds: main 0326637 (`wd-main`), #226 ee6c06b (`wd-ar` + `WARPDRIVE_ASYNC_READ=1`) at its default of 32 workers and with
`WARPDRIVE_HTTP_WORKERS=4` (the coordinator's cores). 4 KB, 128 clients, arms main w32 w4 w4 w32 main. Driver `scripts/hw.sh`.

| 4 KB | main | 32 workers | 4 workers |
|---|---|---|---|
| Single node PUT/s | 1,166, 1,150 | 1,108, 1,120 | 1,166, 1,145 |
| Single node GET/s | 5,456, 5,471 | 5,097, 5,096 | 5,466, 5,483 |
| RS(2,1) PUT/s | 2,574, 2,542 | 2,897, 2,864 | 1,796, 1,802 |
| RS(2,1) GET/s | 6,677, 6,637 | 5,302, 5,298 | 6,639, 6,679 |

- With 4 workers, GET matches main on both topologies; single-node GET p99 drops from 41 ms to 26 ms.
- On the cluster 4 workers cost PUT 38%: PUT's metadata write blocked its worker for a replication round, so the worker
  count capped PUTs in flight. That trade is why the next change made every request path await its I/O (`async-io-ab`).
