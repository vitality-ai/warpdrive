<div align="center">

# WarpDrive - Next Generation Object Storage Engine

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
WarpDrive is a general purpose KV/Object store focused on workloads that require high throughput. Practical Applications which drives our developemt is to support Storage Disaggregated Architectures and AI/ML training Workloads . Our current implementation of the object store is based on Facebook's 2008 haystack paper and our road map([Technical Roadmap](https://github.com/vitality-ai/Storage-service/blob/main/docs/Technical-Roadmap.md)) for our future versions will be driven by the next generation's storage needs with solid fundamental understanding of the history of these storage systems with a product first design. [ v0.1.0 Technical Architecture](https://github.com/vitality-ai/Storage-service/blob/main/docs/Technical-Architecture.md). 

## System Offerings that are currently being built. 
1. Storage - Key/Value, Files and Blobs. 
2. Fault Tolerance - Uses Erasure Coding to Optimise Data replication - Seeks contribution for design - [Discussion](https://github.com/cia-labs/Storage-service/issues/72)
3. User Access Management and S3 middleware - [Repo](https://github.com/vitality-ai/Vitality-console)
4. Search - Seeks contribution for design. -   [Discussion](https://github.com/cia-labs/Storage-service/issues/35)
5. Availability - Seeks contribution for design. [Discussion]()
6. Client Library - Client package is currently available for Python only. [Repo](https://github.com/vitality-ai/python-sdk).
7. Compute and Storage Infrastructure Research - [Repo](https://github.com/vitality-ai/NexCSAD).

---

## Getting Started

See the [User Guide](docs/user_guide.md) for installation, configuration, and API usage examples.

### S3 authentication (Vitality Console)

S3 API requests are authenticated via **Vitality Console** using a single auth path (Console-issued credentials + SigV4). Generate API keys in the Console UI and use them with any S3-compatible client (e.g. boto3).

- **`VITALITY_CONSOLE_URL`** (required): Base URL of Vitality Console (e.g. `http://localhost:8000`). WarpDrive fetches credentials from Console, caches them, and verifies each request's SigV4 signature locally.
- **`WARPDRIVE_SERVICE_SECRET`** (required): Shared secret for `POST {url}/api/auth/s3-credentials`. Must match the value in Console's `.env`.
- Put these in a `.env` file in `server/` (see `server/.env.example`).

---

## Developer's Corner
For more advanced usage and development details, visit the [Developer's Documentation](https://github.com/cia-labs/Storage-service/blob/main/docs/setup.md).
