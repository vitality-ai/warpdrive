# CIAOS Architecture Documentation

## Introduction

CIAOS is a high-throughput key-value/object store optimized for Storage Disaggregated Architectures and AI/ML workloads. The implementation is based on Facebook's 2008 Haystack paper,
focusing on efficient storage and retrieval of objects through a simplified architecture.

## System Architecture - v0.1.0

```mermaid
graph TD
    subgraph "Storage Service"
        subgraph "API Layer"
            API[API Server :9710]
        end

        subgraph "Request Processing"
            SVC[Service Layer]
            BST[Binary Storage Abstraction]
            META[Metadata Storage]
        end

        subgraph "Storage Backends"
            LXFS[LocalXFS Binary Store]
            MOCK[Mock Binary Store]
            FUTURE[Future: CephFS, Lustre, etc.]
        end

        subgraph "Storage Implementation"
            XFS[XFS File System]
            DB[(SQLite Database)]
            MEM[In-Memory Storage]
        end

        API -->|Process Request| SVC
        SVC -->|Store File Data| BST
        SVC -->|Store Metadata| META
        BST -.->|Configuration-based Selection| LXFS
        BST -.->|Testing Backend| MOCK
        BST -.->|Future Backends| FUTURE
        LXFS -->|Single Binary File per User| XFS
        MOCK -->|Testing Only| MEM
        META -->|Key -> Offset/Size Mapping| DB
    end

    C[Client] -->|HTTP Requests| API
```

## Binary Storage Abstraction Layer

The Binary Storage layer has been abstracted behind a trait-based interface to enable:

- **Backend Swapping**: Switch between storage implementations without changing service logic
- **Future Extensibility**: Support for distributed file systems (CephFS, Lustre, JuiceFS)
- **Testing**: Mock implementation for unit testing without disk I/O
- **Configuration**: Environment variable-based backend selection

### Available Backends

- **LocalXFS** (default): Current file-per-user approach with XFS filesystem optimization
- **Mock** (testing): In-memory storage for testing scenarios

### Configuration

The storage backend is selected via the `STORAGE_BACKEND` environment variable:
- `STORAGE_BACKEND=localxfs` or unset: Uses LocalXFS backend  
- `STORAGE_BACKEND=mock`: Uses Mock backend (testing only)
