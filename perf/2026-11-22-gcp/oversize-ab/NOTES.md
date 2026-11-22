# Objects at and past the segment size

Question: what happens when a shard is as large as a segment (64 MB) or larger? Segments give such a shard a file of its
own; one file per bucket appends it like any other write.

Builds: one file b5897db, segments 823f04d (both on #226 async). RS(2,1), 2 clients, 60 s PUT then 60 s GET per run,
arms alternated, objects of 64, 128, 256 and 500 MB (shards half that). Driver `scripts/oversize.sh`.

| Object (shard) | PUT, one file | PUT, segments | GET, one file | GET, segments |
|---|---|---|---|---|
| 64 MB (32 MB) | 1.01 s | 1.01 s | 0.61 s | 0.59 s |
| 128 MB (64 MB) | 3.04 s | **2.04 s** | 1.29 s | 1.27 s |
| 256 MB (128 MB) | 5.74 s | **4.00 s** | 2.82 s | 2.60 s |
| 500 MB (250 MB) | 8.66 s | 8.64 s | **253 s** | **254 s** |

(means of two runs per arm; `summary.csv` has every run)

## Findings

1. **Segments are faster for shards of a segment or more: 33% at 128 MB objects, 30% at 256 MB.** Hypothesis, tested
   in `micro-fsync`: with one file, both writers' shards land in the same file, so each write's fsync also flushes the
   other's dirty pages; a shard with its own segment file flushes only itself. At 500 MB both layouts are limited by
   something else (network and memory: the coordinator holds about 3.5x each object).
2. **GET of a blob over 256 MB is quadratic** in both layouts: 500 MB took about 254 s. The blob cache refuses a blob
   over its 256 MB budget, and each of the GET's 63 slices of 8 MB re-fetches every shard and decodes the whole blob
   (about 47 GB moved per GET). Issue #230. 256 MB objects escape it only because 256 MiB is exactly the budget.
3. **One PUT error per 500 MB run, both layouts**: "connection closed before message completed" on the client, with no
   server error or panic: the keep-alive race (the server closes an idle pooled connection as the client reuses it),
   here while the other client's 4-minute GET held things up.
4. A single PUT of more than about 512 MB fails on RS(2,1): the 256 MB gRPC message cap per shard (#228).
