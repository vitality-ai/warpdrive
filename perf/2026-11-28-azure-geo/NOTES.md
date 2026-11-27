# Geo-distributed cluster on Azure: 5 regions, node 0 in India

Question: how does a 5-node RS(3,2) cluster behave when each node is in a different region, and what does adaptive
compression (`auto`) do there?

## Setup

| Role | Region | Size | CPU |
|---|---|---|---|
| node 0 | Central India | D4ds_v5 (4 vCPU) | Intel Xeon Platinum |
| node 1 | Qatar Central | D4ads_v7 (4 vCPU) | AMD EPYC 9V45 |
| node 2 | East Asia | D4ads_v7 (4 vCPU) | AMD EPYC 9V45 |
| node 3 | Japan East | D4ads_v7 (4 vCPU) | AMD EPYC 9V45 |
| node 4 | Korea Central | D4ads_v7 (4 vCPU) | AMD EPYC 9V45 |
| client | South India | D4ds_v5 (4 vCPU) | Intel Xeon Platinum |

Data on local disk: the AMD nodes' local NVMe (ext4, noatime), the Intel nodes' local temp SSD.

The regions were forced by the Azure Free Trial: 4 vCPUs per region, and many sizes refused for the subscription.
West India, UK South, Germany West Central and France Central offered no 4 vCPU size. Southeast Asia and UAE North
offered only FX sizes. West Europe refused the request (`RequestDisallowedByAzure`). So the client sits in South India,
the closest region with room.

Build `wd-z` (6d78d92a, `feat-pluggable-frontends` with adaptive compression), `s3_bench` `s3b-z` (307f9b6c). RocksDB
metadata (1 GB cache), every other setting default. Fresh cluster per run. 60 s PUT then 60 s GET from the client to
node 0, `S3B_DATA=text` (JSON lines, zstd keeps about a fifth). Driver `scripts/geo2.sh`, derived numbers
`derived.csv` (`../2026-11-27-gcp-compression/scripts/derive.py`). One run per cell, 0 errors everywhere.

## Round trips

TCP connect time in ms, best of 5, rows from, columns to:

| from \ to | Central India | Qatar | East Asia | Japan | Korea |
|---|---|---|---|---|---|
| Central India (node 0) | 0.6 | 39.8 | 76.5 | 116.0 | 107.5 |
| Qatar | 39.9 | 1.7 | 122.0 | 164.3 | 154.9 |
| East Asia | 76.2 | 121.9 | 0.9 | 46.0 | 35.8 |
| Japan | 115.7 | 161.9 | 46.3 | 1.0 | 26.2 |
| Korea | 106.5 | 152.2 | 35.7 | 23.4 | 0.7 |
| client (South India) | 15.8 | 61.3 | 68.2 | 106.0 | 94.0 |

## Results

| Size x clients | PUT/s off / auto | PUT p50 off | GET/s off / auto | GET p50 off | Node 0 CPU (PUT / GET) |
|---|---|---|---|---|---|
| 4 KB x 64 | 213.7 / 216.5 | 284 ms | 2,783 / 2,761 | 14 ms | 6% / 12% |
| 64 KB x 64 | 205.6 / 204.1 | 294 ms | 491 / 492 | 129 ms | 9% / 13% |
| 1 MB x 32 | 105.4 / 100.5 | 289 ms | 236 / 209 | 132 ms | 17% / 23% |

The other four nodes stayed at 0-1% CPU in every run.

## Findings

- **Nothing saturated. Latency sets the rate.** Throughput is clients over latency: 64 clients at about 290 ms per PUT
  is about 220 PUT/s. CPU and network were nearly idle.
- **A PUT costs about 290 ms whatever its size.** The write quorum is 3 of 5. From node 0 the third copy is East Asia,
  76 ms away. A PUT waits for shards and then for the metadata row, each a quorum round trip, plus the client's 16 ms.
- **A 4 KB GET is served locally in 14 ms**, about the client's round trip. The value is inline in the metadata row,
  and every node holds every row.
- **A 64 KB or 1 MB GET takes about 130 ms: it waits for the slowest region.** That is Japan's 116 ms plus the
  client's 16 ms. A read needs any 3 of the 5 shards, so it could finish after East Asia (76 ms). It waits for all
  five instead (the GET fetches every shard and decodes after all have answered, open PR #171). In a geo cluster this
  costs about 40 ms per read.
- **Auto stayed raw, as designed.** CPU (13-23% busy) and network (3-41%) both had room, and on an idle node the cost
  function sends raw. It compressed only the 1-in-32 samples (3%). The −5% PUT and −12% GET at 1 MB came from that one
  run's tail (GET p99 358 ms against 188 ms for off) and need a rerun to mean anything.
- **What auto doesn't see here is latency.** In a geo cluster time goes to round trips, not bytes. Compressing a 1 MB
  blob doesn't shorten a 76 ms round trip, so raw is right here. But nothing in the cost function knows that yet.

## Next

- Finish a read after the first `k` shards arrive (#171). Here that saves about 40 ms per GET.
- Pick the write and read quorums by distance: the nearest `k` regions, not every node.
