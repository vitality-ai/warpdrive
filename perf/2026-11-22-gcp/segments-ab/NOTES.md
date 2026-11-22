# Segments against one file per bucket

Question: does RFC 6 step 3 (64 MB segment files) cost throughput against one growing file per bucket?

Builds: one file b5897db, segments 823f04d (the commit after it), both on #226 async. Arms segbase seg seg segbase.
Driver `scripts/seg.sh`.

| | One file | Segments |
|---|---|---|
| RS(2,1) 4 KB PUT/s | 3,825, 3,813 | 3,880, 3,865 |
| RS(2,1) 4 KB GET/s | 6,481, 6,474 | 6,499, 6,476 |
| RS(2,1) 1 MB PUT/s | 230, 232 | 230, 259 |
| RS(2,1) 1 MB GET/s | 490, 491 | 488, 567 |
| Single node 4 KB PUT/s | 1,765, 1,762 | 1,782, 1,764 |
| Single node 4 KB GET/s | 5,491, 5,549 | 5,521, 5,530 |
| Single node 1 MB PUT/s | 290, 290 | 292, 288 |
| Single node 1 MB GET/s | 1,148, 1,147 | 1,152, 1,149 |

No regression anywhere; differences are within run-to-run noise (up to 13% at 1 MB between runs of one arm).
