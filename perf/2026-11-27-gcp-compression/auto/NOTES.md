# Blob compression: off, zstd on every blob, and auto

Question: does `auto` (compress a blob when this node's measured CPU, network, disk and read rate say it pays,
`cluster/adaptive.rs`) keep zstd's gains without its losses?

Build: `feat-pluggable-frontends` (`wd-z`, 6d78d92a), one binary for all arms: `WARPDRIVE_BLOB_COMPRESSION` unset
(off), `zstd`, or `auto`. Setup, payloads and measurements as `../static-zstd`. Driver `../scripts/zs2.sh` (arms run
off, zstd, auto at each size, each on a fresh cluster). `derived.csv` from `../scripts/derive.py`. Auto's own view of
node 0 at the end of each run: `raw/*-auto-*-decisions.txt`. One run per cell, 0 errors everywhere.

| Data, size | PUT/s off / zstd / auto | GET/s off / zstd / auto | Blobs auto compressed |
|---|---|---|---|
| text 64 KB | 1,191 / 1,094 / 1,194 | 2,050 / 2,122 / 2,045 | 3% |
| text 1 MB | 176 / 175 / 177 | 330 / **420** / 335 | 3% |
| text 8 MB | 18.5 / 16.8 / 19.4 | 39.5 / **54.0** / 39.9 | 3% |
| random 64 KB | 1,177 / 1,061 / 961 | 2,055 / 1,824 / 1,667 | 0% |
| random 1 MB | 177 / 177 / 177 | 329 / 332 / 332 | 0% |
| random 8 MB | 19.4 / 18.8 / 19.7 | 39.7 / 39.8 / 39.7 | 0% |

Auto's view of node 0 at the end of the text runs: CPU 83 to 99% busy, network 9 to 29% of a 10 Gbit/s link, zstd at
115 to 190 MB/s under load, ratio 0.2, about 1.9 reads per write.

## Reruns: random 64 KB (`../auto-reruns`)

| Order | auto PUT/s | off PUT/s |
|---|---|---|
| auto first | 1,197 | 1,204 |
| off first | 1,008 | 1,210 |
| this run (auto third) | 961 | 1,177 |

## Findings

- **Auto matches off on text at every size.** It avoids zstd's PUT losses (−8% at 64 KB, −9% at 8 MB here).
- **Auto misses zstd's GET gains** (+27% at 1 MB, +37% at 8 MB). With node 0's CPU near full and the network
  measured as idle, the cost function says compressing spends what's short to save what isn't. Two things it doesn't
  see yet: the shard bytes node 0 receives on a GET (it counts only bytes sent), and the CPU fewer bytes save
  (Reed-Solomon, shard I/O, on every node).
- **Random 64 KB is open.** Auto ran 17% below off in two of three runs, both when it ran after another arm. Auto
  compresses 1 blob in 32 there and nothing else. Not explained yet.

## Decision

Auto ships, opt-in. The default stays off until auto counts received bytes and the CPU that compression saves, and
then matches or beats off and zstd everywhere in this table.
