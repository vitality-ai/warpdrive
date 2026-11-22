# GCP runs, 2026-11-22: #226 async I/O, RFC 6 segments and compaction

One folder per run. Each holds `raw/` (the driver's own output: progress log, per-run `bench.csv`, client and node samples,
node logs), `summary.csv`, plots, and `NOTES.md` with the question, the setup and the result. `scripts/` holds every
driver as run, the samplers, and the analysis and plot scripts.

## Setup

- GCP us-central1: `wln-0` n2-standard-4, `wln-1`/`wln-2` n2-standard-2, each with one local NVMe (ext4 partition at
  `/mnt/nvme`); client `wlc` n2-standard-4. RS(2,1) on the 3 nodes, or a single node on `wln-0`.
- Logging at WARN. 60 s per phase unless noted; arms alternate within one run (A B B A), so VM-to-VM drift cancels.
- Builds come from commit-pinned tarballs (`scripts/pack.sh`: `git archive` of a commit plus a `COMMIT` file); every
  progress log records the commit and the binary's sha256.

## Re-running

From a Mac with the VMs up: copy a tarball and its driver to `wlc` (`scripts/lib.sh` has `rcp`/`rs`), start the driver
detached (`setsid nohup bash ~/<driver>.sh`), then pull `~/<run>/`. Plots: `scripts/plot_ab.py` for A/B summaries,
`scripts/analyze_comp.py` then `scripts/plot_comp.py` for compaction runs.

## Runs

| Folder | Question |
|---|---|
| `workers-ab` | Do 32 HTTP workers on 4 cores cost #226 its GETs? |
| `get-cpu-profile` | Where does #226's GET CPU go, against main? |
| `async-io-ab` | With every request path awaiting its I/O, does #226 match main? |
| `segments-ab` | Do segment files cost throughput, against one file per bucket? |
