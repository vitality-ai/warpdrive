# #226 async I/O against main

Question: with every request path awaiting its I/O and one HTTP worker per core, does #226 match main?

Builds: main 0326637, #226 before the change (ee6c06b, 32 workers), #226 async (b919f76: PUT and GET await their I/O,
workers = cores; later commits on #226 convert the rarer paths the same way). 60 s per phase, arms main ar as as ar main.
Driver `scripts/as.sh`. Means of the two runs per arm, all 0 errors on #226:

| | main | before async | async |
|---|---|---|---|
| RS(2,1) 4 KB PUT/s | 2,563 | 2,826 | **3,659** |
| RS(2,1) 4 KB GET/s | 6,662 | 5,275 | 6,241 |
| RS(2,1) 1 MB PUT/s | 222 | 262 | 241 |
| RS(2,1) 1 MB GET/s | 369 | 575 | 523 |
| Single node 4 KB PUT/s | 1,772 | 1,710 | 1,775 |
| Single node 4 KB GET/s | 5,522 | 5,254 | 5,532 |
| Single node 1 MB PUT/s | 311 | 356 | 287 |
| Single node 1 MB GET/s | 1,092 | 908 | 1,147 |

- Cluster 4 KB PUT is 43% over main; 4 KB GET is 6% under main, up 18% from before.
- Single-node 4 KB matches main, with GET p99 back to 28 ms from 59 ms.
- Under main: cluster 4 KB GET by 6% and single-node 1 MB PUT by 8%.
