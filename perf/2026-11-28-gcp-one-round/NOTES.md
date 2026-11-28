# One-round PUT (#252, feat-one-round-put at 326283c) against #250

Question: does sending a PUT's shards with its metadata row (one round trip instead of two) speed up PUTs, and is
it correct, including under node crashes?

Builds on `wlc`: `wd-base` (#250 at 4891e3f) and `wd-1r` (326283c), default flags. 5 x n2-standard-2 (local
NVMe), RS(3,2), RocksDB metadata, every request to node 0. Arms:

| Arm | Build and setting | PUT |
|---|---|---|
| two-all | wd-base | shards, then row, each round waits for every peer |
| two-quorum | wd-1r, `WARPDRIVE_BLOB_WITH_ROW=off` | shards (returns at k), then row |
| one | wd-1r (default) | shards ride with the row, waits for every peer |
| one-quorum | wd-1r, `WARPDRIVE_ROW_ACK=quorum` | shards with the row, returns at k including node 0 |

Scripts in `scripts/`: `or1.sh` (throughput, with `geo_netem.sh` for the Azure round trips), `ver.sh` and
`verify.py` (byte-for-byte), `fault.sh` and `loader.py` (crashes).

## Throughput

`zone/`: same zone, 20 s PUT then 20 s GET, 1 run each, 0 errors. PUT ops/s:

| Size, clients | two-all | two-quorum | one | one-quorum |
|---|---|---|---|---|
| 4 KB, 64 | 1,577 | 1,652 | 3,224 | 3,307 |
| 64 KB, 64 | 1,159 | 1,140 | 1,732 | 1,732 |
| 1 MB, 32 | 175 | 177 | 176 | 178 |

`geo-partial/`: the Azure round-trip matrix added with netem, 60 s phases, stopped after 14 of 24 cells (geo runs
move to Azure). 0 errors. PUT ops/s, run 1:

| Size, clients | two-all | two-quorum | one | one-quorum |
|---|---|---|---|---|
| 4 KB, 64 | 205 | 257 | 307 | 458 |
| 64 KB, 64 | 207 | 252 | 250 | 422 |
| 1 MB, 32 | 97 | 109 | 105 | 157 |

1 MB PUTs are capped at about 176/s in every arm on one zone. The server's own timing puts a PUT at about 15 ms
against a 170 ms client p50, so the cap is outside the handler (the client's upload or the request reaching node 0).

GET numbers here are not comparable between arms. GETs fill a 256 MB read cache on first read, and arms that wrote
more objects in the PUT phase spend more of the GET phase on first reads (133 ms each on geo). A GET comparison
needs a fixed object count.

## Correctness

`verify/`: per arm, 249 objects (0 B to 8 MB and a 3-part multipart upload) of random bytes PUT through node 0,
read back from all 5 nodes, then through node 0 with node 4 stopped. 0 mismatches and 0 errors in every arm, 0
panics.

`fault/`: per arm, 40 s of 16 PUT threads (4 KB, 64 KB, 1 MB) and 8 GET threads that read acked objects and compare
SHA-256, all through node 0. SIGKILL at 10 s, restart with the same data at 25 s, then every acked object is read
from every node. 0 panics.

| Scenario | Arm | PUTs acked / failed | GETs during the run: ok / wrong / error | Acked objects missing after restart, nodes 0-4 | Failed PUTs visible, nodes 0-4 |
|---|---|---|---|---|---|
| node 3 crashes | two-all | 5,911 / 0 | 3,442 / 0 / 0 | 0, 0, 0, 2,383, 0 | 0 |
| | one | 6,008 / 0 | 3,514 / 0 / 0 | 0, 0, 0, 2,423, 0 | 0 |
| | one-quorum | 5,970 / 0 | 3,437 / 0 / 0 | 0, 0, 0, 2,391, 0 | 0 |
| nodes 2, 3, 4 crash | two-all | 3,545 / 1,040 | 2,510 / 0 / 600 | 0, 0, 59, 170, 287 | 0 |
| | one | 3,549 / 1,097 | 2,493 / 0 / 611 | 0, 0, 66, 159, 275 | 0 |
| | one-quorum | 3,498 / 1,134 | 2,535 / 0 / 612 | 0, 0, 69, 134, 227 | 0 |
| node 0 crashes | two-all | 3,615 / 1,201 | 2,160 / 0 / 615 | 0 | 0 |
| | one | 3,577 / 1,202 | 2,125 / 0 / 624 | 0 | 0 |
| | one-quorum | 3,577 / 1,202 | 2,166 / 0 / 619 | 0 | 0, 1, 0, 1, 1 |

Findings:

1. **No GET returned wrong bytes, in any arm or scenario.** GET errors happen only while node 0 is down or fewer
   than k nodes are up.
2. **One round behaves the same as #250 under every crash.** Every difference between arms is within run noise.
3. **A restarted node is missing the rows written while it was down, in #250 too.** A GET through it returns
   `NoSuchKey` for an acked object. Nodes 2, 3 and 4 restart one after another, which probably explains their
   rising counts there (later starts miss more writes); not checked key by key. Rows have no catch-up when a
   node rejoins. Issue #251 (shard repair) does not cover this: the row is missing, so the node never looks
   for shards.
4. **One PUT cut off by the coordinator crash is visible on 3 nodes in one-quorum.** The client got no reply, so
   S3 allows either outcome. Nodes 0 and 2 don't have it, which is the same missing catch-up as 3.
