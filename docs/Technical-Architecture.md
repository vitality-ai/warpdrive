# WarpDrive Architecture Documentation

## Introduction

WarpDrive is a high-throughput key-value/object store optimized for Storage Disaggregated Architectures and AI/ML workloads. The implementation is based on Facebook's 2008 Haystack paper,
focusing on efficient storage and retrieval of objects through a simplified architecture.

## System Architecture - v0.1.0


```mermaid
graph TD
    subgraph "Storage Service"
        subgraph "API Layer"
            API[API Server :9710]
            NATIVE[Native CIAOS API]
            S3API[S3-Compatible API]
        end

        subgraph "Request Processing"
            SVC[Service Layer]
            S3HANDLER[S3 Handlers]
            UNIFIED[Unified Storage Interface]
            BIN[Binary Storage]
            META[Metadata Storage]
        end

        subgraph "Storage Implementation"
            XFS[XFS File System]
            DB[(SQLite Database)]
        end

        API -->|Native Requests| NATIVE
        API -->|S3 Requests| S3API
        NATIVE -->|Process Request| SVC
        S3API -->|Process S3 Request| S3HANDLER
        SVC -->|Unified Storage| UNIFIED
        S3HANDLER -->|Unified Storage| UNIFIED
        UNIFIED -->|Store File Data| BIN
        UNIFIED -->|Store Metadata| META
        BIN -->|Single Binary File per User| XFS
        META -->|Key -> Offset/Size Mapping| DB
    end

    C[Client] -->|HTTP Requests| API
    S3CLIENT[S3 Client] -->|boto3/aws-cli| S3API
```

