<div align="center">
  <img src="assets/warpdrive-logo.svg" alt="WarpDrive" width="420">

  <p><strong>Object storage for systems and the agentic era</strong></p>
  <p>Fast by default. Explainable SLAs. Safely customizable.</p>

  <p>
    <a href="https://vitality-ai.github.io/warpdrive/site/"><img alt="Website" src="https://img.shields.io/badge/Website-4a0e63?style=for-the-badge&logo=googlechrome&logoColor=ffb366&labelColor=4a0e63"></a>
    <a href="https://discord.gg/ZrxZnE87X"><img alt="Discord" src="https://img.shields.io/badge/Discord-Join-5865F2?style=for-the-badge&logo=discord&logoColor=white&labelColor=1a1025"></a>
    <a href="https://github.com/vitality-ai/warpdrive/releases"><img alt="Version" src="https://img.shields.io/badge/version-1.0.0--beta-ff8a3d?style=for-the-badge&logo=semver&logoColor=1a1025&labelColor=1a1025"></a>
    <a href="https://github.com/vitality-ai/warpdrive/stargazers"><img alt="Stars" src="https://img.shields.io/github/stars/vitality-ai/warpdrive?style=for-the-badge&logo=star&color=ff8a3d&logoColor=1a1025&labelColor=1a1025"></a>
    <a href="https://github.com/vitality-ai/warpdrive/network/members"><img alt="Forks" src="https://img.shields.io/github/forks/vitality-ai/warpdrive?style=for-the-badge&logo=git-fork&color=c92a6b&logoColor=white&labelColor=1a1025"></a>
    <a href="https://github.com/vitality-ai/warpdrive"><img alt="Rust" src="https://img.shields.io/badge/Rust-98.6%25-CE422B?style=for-the-badge&logo=rust&logoColor=white&labelColor=1a1025"></a>
    <a href="https://github.com/vitality-ai/warpdrive/commits/main"><img alt="Last Commit" src="https://img.shields.io/github/last-commit/vitality-ai/warpdrive?style=for-the-badge&logo=clock&color=b06bd9&logoColor=1a1025&labelColor=1a1025"></a>
  </p>
</div>

📄 **Position Paper (Vision):** [*WarpDrive: A Composable Storage Substrate for Disaggregated Data Systems*](https://vldb.org/2026/Workshops/VLDB-Workshops-2026/CDMS/CDMS26_8.pdf), accepted as a **lightning talk at CMDS (Composable Data Management Systems), the VLDB 2026 workshop**.

---

## About

WarpDrive is purpose-built for high-throughput workloads: storage-disaggregated architectures and data-intensive distributed systems. Our broader aim is storage primitives built with a deep understanding of the backend underneath them, making computational pushdown and storage-centric execution first-class. That lets data systems, ML frameworks, and agentic workflows move computation closer to data: less unnecessary movement, more efficient large-scale processing, retrieval, and orchestration.

WarpDrive is an object store that's fast by default, explains its own performance well enough to back an SLA, and lets you customize data placement without risking availability.

Run it **single-node** for a fully S3-compatible store in one process, a drop-in for local development and embedded use (see compatibility results below). Run it **distributed** for a multi-node, erasure-coded engine: no single coordinator, no leader election, quorum-based reads/writes.

What makes WarpDrive different is **content-dependent placement**: a bucket can opt a workload's own structure (Parquet column chunks, vector-index partitions) into how its bytes are striped and erasure-coded, instead of treating every object as an opaque blob. The system reports the cost of that choice before it risks availability.

- **149x faster** on a selective official TPC-H query (DuckDB, via its own `dbgen`): pushdown skips untouched stripes instead of reconstructing the whole object.
- **Up to 41x faster** on Lance vector search (`IVF_PQ`) `take` latency, recall@10 identical to the unpacked baseline on SIFT1M-small. Speed with no accuracy trade-off.

Both numbers are WarpDrive-packed vs. WarpDrive-plain on a single local cluster, RS(3,2), not a comparison against MinIO or against Fusion's own RS(9,6) parameter.

Full writeup, plots, and methodology: [`docs/benchmarks/v1.0.0-results.md`](docs/benchmarks/v1.0.0-results.md). Architecture for both modes: [Technical Architecture](docs/Technical-Architecture.md). Our longer-term direction: [Technical Roadmap](docs/Technical-Roadmap.md).

---

## Getting Started

See the [User Guide](docs/user_guide.md) for installation, configuration, and API usage examples.

## Performance Benchmarks

WarpDrive's content-dependent placement is measured against real tools and real data, not synthetic benchmarks. Full methodology, more queries/nprobe levels, and plots in [`docs/benchmarks/v1.0.0-results.md`](docs/benchmarks/v1.0.0-results.md).

| System | Type | Workload | Result |
|--------|------|----------|--------|
| [DuckDB](https://duckdb.org) | Analytical SQL engine | Official TPC-H (via `dbgen`), selective query (Q6) | **149x faster** (7756.6ms → 51.9ms) via row-group pushdown |
| [Lance](https://lancedb.github.io/lance/) | Vector search (`IVF_PQ`) | `take` latency, 20k vectors/768-dim | **Up to 41x faster**, recall@10 identical to baseline |
| Lance | Vector search (`IVF_PQ`) | SIFT1M-small (published benchmark corpus) | **Up to 5.5x faster**, recall@10 identical to baseline |

## Compatibility Tests

WarpDrive is tested against real-world storage clients and databases to validate S3 compatibility. These are single-node `/s3/` API tests, separate from the distributed-engine benchmarks above. Full results in [`docs/compatibility_tests/`](docs/compatibility_tests/).

| System | Type | Version Tested | Status | Notes |
|--------|------|---------------|--------|-------|
| [TidesDB](https://tidesdb.com) | Embedded LSM KV store | C library v9.3.6 / Rust crate 0.11.1 | ✅ Passing | [Full report](docs/compatibility_tests/tidesdb.md): object store mode, replication, 17/17 CI tests pass |
| [SlateDB](https://slatedb.io) | Embedded LSM KV store | slatedb 0.14 / object_store 0.14 | ✅ Passing | [Full report](docs/compatibility_tests/slatedb.md): 1000-key write/flush/read/range-scan/delete, ISO 8601 LastModified required |
| [Neon](https://neon.tech) | Serverless Postgres | neon main / aws-sdk-rust 1.3.3 | ✅ Passing | [Full report](docs/compatibility_tests/neon.md): pageserver + safekeeper backed by WarpDrive, full Postgres write/read verified |

**In pipeline:**

| System | Type |
|--------|------|
| [LangGraph](https://langchain-ai.github.io/langgraph/) | Agentic workflow orchestration |
| [LlamaIndex](https://www.llamaindex.ai) | RAG / agentic data framework |
| [Ray](https://ray.io) | Distributed ML training & serving |
| [PyTorch Lightning](https://lightning.ai) | ML training checkpointing |

## Developer's Corner
For more advanced usage and development details, visit the [Developer's Documentation](docs/setup.md).
