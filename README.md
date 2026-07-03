<div align="center">

# WarpDrive 
## Next Generation Object Storage Engine

<img src="https://github.com/user-attachments/assets/654f3add-74ab-4c34-8b73-234852ea11c7" alt="Storage Service Banner" width="800" height="250">

<br><br>

[![Stars](https://img.shields.io/github/stars/vitality-ai/Storage-service?style=for-the-badge&logo=star&color=FFD700&logoColor=000000&labelColor=1a1a1a)](https://github.com/vitality-ai/Storage-service/stargazers) 
[![Forks](https://img.shields.io/github/forks/vitality-ai/Storage-service?style=for-the-badge&logo=git-fork&color=4A90E2&logoColor=white&labelColor=1a1a1a)](https://github.com/vitality-ai/Storage-service/network/members) 
[![Issues](https://img.shields.io/github/issues/vitality-ai/Storage-service?style=for-the-badge&logo=bug&color=FF4444&logoColor=white&labelColor=1a1a1a)](https://github.com/vitality-ai/Storage-service/issues)
[![License](https://img.shields.io/github/license/vitality-ai/Storage-service?style=for-the-badge&logo=law&color=32CD32&logoColor=white&labelColor=1a1a1a)](https://github.com/vitality-ai/Storage-service/blob/main/LICENSE)
[![Rust](https://img.shields.io/badge/Rust-98.6%25-CE422B?style=for-the-badge&logo=rust&logoColor=white&labelColor=1a1a1a)](https://github.com/vitality-ai/Storage-service) 
[![Last Commit](https://img.shields.io/github/last-commit/vitality-ai/Storage-service?style=for-the-badge&logo=clock&color=9966CC&logoColor=white&labelColor=1a1a1a)](https://github.com/vitality-ai/Storage-service/commits/main)

</div>


## About
WarpDrive is a purpose-built KV/Object store focused on workloads that demand high throughput. Practical applications driving our development are storage-disaggregated architectures and data-intensive distributed systems.
Our broader aim is to build storage primitives and interfaces with a deep understanding of the underlying backend architecture, making computational pushdown and storage-centric execution first-class capabilities. By exposing these abstractions cleanly, we aim to simplify how data systems, ML frameworks, and agentic workflows move computation closer to data, reducing unnecessary data movement while enabling efficient large-scale processing, retrieval, and orchestration. Our road map([Technical Roadmap](https://github.com/vitality-ai/Storage-service/blob/main/docs/Technical-Roadmap.md)) for our future versions will be driven by the next generation's storage needs with solid fundamental understanding of the history of these storage systems with a product first design. [ v0.1.0 Technical Architecture](https://github.com/vitality-ai/Storage-service/blob/main/docs/Technical-Architecture.md). 

## System Offerings that are currently being built. 
1. Storage - Key/Value, Files and Blobs. 
2. Fault Tolerance - Uses Erasure Coding to Optimise Data replication - Seeks contribution for design - [Discussion](https://github.com/cia-labs/Storage-service/issues/72)
3. User Access Management  - [Repo](https://github.com/vitality-ai/Vitality-console)
4. Search - Seeks contribution for design. -   [Discussion](https://github.com/cia-labs/Storage-service/issues/35)
5. Availability - Seeks contribution for design. [Discussion]()
6. Client Library - S3 compatible/ Custom Client package is currently available for Python only. [Repo](https://github.com/vitality-ai/python-sdk).
7. Compute and Storage Infrastructure Research - [Repo](https://github.com/vitality-ai/NexCSAD).

---

## Getting Started

See the [User Guide](docs/user_guide.md) for installation, configuration, and API usage examples.

## Compatibility Tests

Warpdrive is tested against real-world storage clients and databases to validate S3 compatibility. Results are documented in [`docs/compatibility_tests/`](docs/compatibility_tests/).

| System | Type | Version Tested | Status | Notes |
|--------|------|---------------|--------|-------|
| [TidesDB](https://tidesdb.com) | Embedded LSM KV store | C library v9.3.6 / Rust crate 0.11.1 | ✅ Passing | [Full report](docs/compatibility_tests/tidesdb.md) — object store mode, replication, 17/17 CI tests pass |
| [SlateDB](https://slatedb.io) | Embedded LSM KV store | slatedb 0.14 / object_store 0.14 | ✅ Passing | [Full report](docs/compatibility_tests/slatedb.md) — 1000-key write/flush/read/range-scan/delete, ISO 8601 LastModified required |
| [Neon](https://neon.tech) | Serverless Postgres | neon main / aws-sdk-rust 1.3.3 | ✅ Passing | [Full report](docs/compatibility_tests/neon.md) — pageserver + safekeeper backed by Warpdrive, full Postgres write/read verified |

**In pipeline:**

| System | Type |
|--------|------|
| [LangGraph](https://langchain-ai.github.io/langgraph/) | Agentic workflow orchestration |
| [LlamaIndex](https://www.llamaindex.ai) | RAG / agentic data framework |
| [Ray](https://ray.io) | Distributed ML training & serving |
| [PyTorch Lightning](https://lightning.ai) | ML training checkpointing |

## Developer's Corner
For more advanced usage and development details, visit the [Developer's Documentation](https://github.com/cia-labs/Storage-service/blob/main/docs/setup.md).
