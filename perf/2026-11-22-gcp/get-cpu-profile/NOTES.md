# CPU per GET on the coordinator

Question: where does #226's GET CPU go, against main? RS(2,1), 4 KB, 128 clients, 90 s PUT then 90 s GET; in the GET
phase at +30 s on node0: `perf record -g --call-graph dwarf` for 15 s, then `perf stat` and `pidstat` for 10 s.
Driver `scripts/getprof.sh`. Flat, by-library and inclusive reports plus folded stacks in `raw/<arm>/`.

| Build | GET/s | Cores busy | CPU µs per GET | Context switches per GET |
|---|---|---|---|---|
| main 0326637 | 6,645 | 3.67 | 552 | 4.1 |
| #226 ee6c06b (32 workers) | 5,232 | 3.76 | 719 | 9.0 |

#226 did 2.2 times the context switches per GET and spent 30% more CPU on each: threads parked on network I/O handing
off to each other (blocking pool, bridge thread, bridge runtime). This is the measurement behind the async change.
