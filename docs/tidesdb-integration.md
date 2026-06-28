# Using TidesDB with Warpdrive

> **Note:** This guide was written and validated against a locally built and locally running Warpdrive instance. Warpdrive was compiled from source (`cargo build --release`), started on `localhost:9710`, and used as the S3 backend for TidesDB — no cloud account or external service required.

TidesDB is an embedded LSM-tree key-value storage engine with an **object store mode** that uploads its internal files (SSTables, WAL, manifests) to an S3-compatible backend. Warpdrive works as that backend out of the box.

## How it works

```
Your Application
      │
      ▼
   TidesDB  (embedded, in-process)
      │
      │  S3 API  (HTTP)
      ▼
   Warpdrive  (localhost:9710)
```

When TidesDB is opened in object store mode it:
1. Buffers writes in a local memtable and write-ahead log
2. On flush/compaction, serializes data into SSTable files and uploads them to Warpdrive as S3 objects
3. Serves reads from local cache; downloads from Warpdrive on a cache miss
4. On restart, recovers its full state from the bucket — the local directory is just a cache

## Prerequisites

**TidesDB** — refer to the [official TidesDB documentation](https://tidesdb.com) for full setup instructions for your language and platform. The language-specific API references (Rust, Go, Python, Java, etc.) are at [tidesdb.com/reference](https://tidesdb.com/reference/rust/).

For object store mode specifically, TidesDB's S3 connector requires libcurl and openssl in addition to the standard build dependencies. On Ubuntu/Debian:

```bash
# Base build tools + compression libs (required for all TidesDB builds)
sudo apt-get install -y cmake pkg-config build-essential \
  libzstd-dev liblz4-dev libsnappy-dev

# S3 object store support (required for Warpdrive integration)
sudo apt-get install -y libcurl4-openssl-dev libssl-dev
```

> **cmake version:** Ubuntu 22.04 ships cmake 3.22 but TidesDB requires 3.25+. We worked around this by installing a newer cmake via pip (`pip3 install cmake`) and putting the venv bin directory first on `PATH` before building.

In `Cargo.toml`, enable the `objectstore` feature — this activates the S3 connector and pulls in the libcurl/openssl bindings:

```toml
[dependencies]
tidesdb = { version = "0.11", features = ["objectstore"] }
```

On first `cargo build`, the TidesDB C library is automatically downloaded and compiled from source.

**Warpdrive running:**

```bash
cd warpdrive/server
WARPDRIVE_ADMIN_ACCESS_KEY=adminkey \
WARPDRIVE_ADMIN_SECRET_KEY=adminsecretkey123456 \
  ./target/release/warp_drive
```

Listens on port **9710**.

## Creating the bucket

Since Warpdrive is S3-compatible, use the AWS CLI to create the bucket:

```bash
AWS_ACCESS_KEY_ID=adminkey \
AWS_SECRET_ACCESS_KEY=adminsecretkey123456 \
AWS_DEFAULT_REGION=us-east-1 \
  aws s3api create-bucket --bucket my-tidesdb-bucket \
    --endpoint-url http://localhost:9710
```

## Example

For this guide I've been working in Rust, so the example below uses the [tidesdb Rust crate](https://tidesdb.com/reference/rust/). That said, TidesDB has bindings for many languages and since Warpdrive is just an S3-compatible HTTP interface, please feel free to set this up in whichever language works best for you — the S3 endpoint, credentials, and config flags are the same regardless.

A runnable version of this demo is in [`demo/tidesdb/`](../demo/tidesdb) in this repository.

```rust
use tidesdb::{TidesDB, Config, ColumnFamilyConfig, LogLevel, ObjectStoreConfig, S3Config};

let s3 = S3Config::new("localhost:9710", "my-tidesdb-bucket", "adminkey", "adminsecretkey123456")
    .region("us-east-1")
    .use_path_style(true)  // required — Warpdrive uses path-style URLs
    .use_ssl(false);       // Warpdrive is plain HTTP

let db = TidesDB::open(
    Config::new("./local-cache")
        .object_store_s3(s3)
        .object_store_config(ObjectStoreConfig::new())
        .log_level(LogLevel::Info),
)?;

// Create a column family
db.create_column_family("my-cf", ColumnFamilyConfig::default())?;
let cf = db.get_column_family("my-cf")?;

// Write
let mut txn = db.begin_transaction()?;
txn.put(&cf, b"hello", b"world", -1)?;
txn.commit()?;

// Read
let txn = db.begin_transaction()?;
let val = txn.get(&cf, b"hello")?;
println!("{}", String::from_utf8_lossy(&val)); // "world"
```

To run the full demo (creates bucket, writes 100 pairs, flushes, reads back, lists bucket contents):

```bash
cd demo/tidesdb
PATH="$VENV/bin:$PATH" cargo run
```

## Critical config flags

Both flags must be set explicitly — the defaults do not work with Warpdrive:

| Flag | Default | Required | Reason |
|------|---------|----------|--------|
| `use_path_style(true)` | `false` | Yes | Default is virtual-hosted style (`bucket.host/key`); Warpdrive uses path-style (`host/bucket/key`) |
| `use_ssl(false)` | `false` | Yes | Warpdrive listens on plain HTTP |

Without `use_path_style(true)` TidesDB constructs URLs like `http://my-tidesdb-bucket.localhost:9710/UNIMAP` which Warpdrive cannot route, and all uploads silently fail after 3 retries.

## What gets stored in Warpdrive

After a flush/compaction the bucket will contain:

| Object key | Description |
|------------|-------------|
| `UNIMAP` | Unified memtable index — maps column family names to internal indexes |
| `uwal_N.log` | Write-ahead log segment — contains raw KV entries in write order |
| `<cf>/config.ini` | Column family configuration (compression, bloom filter, etc.) |
| `<cf>/MANIFEST` | SSTable manifest — tracks which SSTable files exist at each level |
| `<cf>/L1P0_N.klog` | Level-1 SSTable key log — sorted, indexed, compressed keys |
| `<cf>/L1P0_N.vlog` | Level-1 SSTable value log |

List them at any time:

```bash
AWS_ACCESS_KEY_ID=adminkey \
AWS_SECRET_ACCESS_KEY=adminsecretkey123456 \
AWS_DEFAULT_REGION=us-east-1 \
  aws s3api list-objects-v2 --bucket my-tidesdb-bucket \
    --endpoint-url http://localhost:9710
```

## Recovery

TidesDB can fully recover from the bucket alone. Delete the local cache directory and reopen with the same config — TidesDB will download the MANIFEST and SSTables from Warpdrive and resume from where it left off.
