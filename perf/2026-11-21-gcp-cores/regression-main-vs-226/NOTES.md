# Regression check: main vs #226

Exact builds: main at 0326637, #226 at b62e763, each built from its commit. node0 n2-standard-4 (all 4 cores), nodes 1-2
n2-standard-2, local NVMe, client n2-standard-4, WARN logging. 60 s PUT then 60 s GET, builds alternated, 2 runs each.
`summary.csv`.

| Setup | PUT/s main -> #226 | GET/s main -> #226 | errors main / #226 |
|---|---|---|---|
| single node, 4 KB, 128 clients | 1,135 -> 1,130 | 5,418 -> 5,106 (-5.8%), p99 29 -> 61 ms | 0 / 0 |
| single node, 1 MB, 32 clients | 282 -> 347 (+23%) | 1,116 -> 1,075 (-3.7%) | 0 / 0 |
| RS(2,1), 4 KB, 128 clients | 2,430 -> 2,880 (+19%) | 6,332 -> 4,724 (-25%) | 86 / 0 |
| RS(2,1), 1 MB, 32 clients | 220 -> 258 (+17%) | 366 -> 549 (+50%) | 0 / 0 |

- #226 is faster or equal on every PUT and on 1 MB cluster GET, with no errors where main had 86.
- **4 KB GET regressed on the cluster (-25%)** and less on single node. A #226 cluster GET crosses the HTTP worker, the
  blocking pool, a bridge thread and the gRPC client. main's cluster GET calls the peer client from its async handler.
