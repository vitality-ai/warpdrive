# Async GET read A/B

Question: does awaiting the shard reads on the HTTP worker close #226's 4 KB GET gap with main?

One binary (b62e763 + `Storage::read_async`, behind `WARPDRIVE_ASYNC_READ` for this run) plus main at 0326637.
node0 n2-standard-4 (all 4 cores), 4 KB, 128 clients, 60 s PUT then 60 s GET, 2 runs each, alternated. `summary.csv`.

| RS(2,1) | GET/s runs | mean | GET p99 | errors |
|---|---|---|---|---|
| main | 6,655, 4,975 | 5,815 | 31-35 ms | 105 |
| #226, switch off | 4,785, 4,747 | 4,766 | 37 ms | 0 |
| #226, switch on | 5,339, 5,342 | 5,341 | 37 ms | 0 |

- The async read adds 12% on cluster GET with tight runs and no PUT cost. It recovers about half the gap with main.
  main is noisy (4,975 to 6,655) and has errors. It became the default in #226 (ee6c06b).
- Single node is unaffected (it reads local disk): main 5,456 GET/s at 28 ms p99, #226 about 5,160 at 60 ms p99
  in both arms. That gap has another cause. Suspect: HTTP workers default to max(32, cores) since b62e763.
