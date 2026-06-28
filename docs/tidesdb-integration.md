# Using TidesDB with Warpdrive

TidesDB is an embedded LSM-tree key-value storage engine with an **object store mode** that uploads its internal files (SSTables, WAL, manifests) to an S3-compatible backend. Warpdrive works as that backend out of the box.

## How it works

```
Your Application
      │
      ▼
   TidesDB  (embedded, in-process)
      │
      │  S3 API  (SigV4-signed, HTTP)
      ▼
   Warpdrive  (localhost:9710)
```

When TidesDB is opened in object store mode it:
1. Buffers writes in a local memtable and write-ahead log
2. On flush/compaction, serializes data into SSTable files and uploads them to Warpdrive as S3 objects
3. Serves reads from local cache; downloads from Warpdrive on a cache miss
4. On restart, recovers its full state from the bucket — the local directory is just a cache

## Prerequisites

**System packages:**

```bash
sudo apt-get install -y cmake pkg-config \
  libzstd-dev liblz4-dev libsnappy-dev \
  libcurl4-openssl-dev libssl-dev build-essential
```

> The `cmake` bundled with Ubuntu 22.04 (3.22) is too old — TidesDB requires 3.25+. Install a newer one via pip:
>
> ```bash
> pip3 install cmake        # installs cmake 4.x into your virtualenv
> export PATH="$VENV/bin:$PATH"   # put it first on PATH before building
> ```

**Warpdrive running:**

```bash
cd warpdrive/server
WARPDRIVE_ADMIN_ACCESS_KEY=adminkey \
WARPDRIVE_ADMIN_SECRET_KEY=adminsecretkey123456 \
  ./target/release/warp_drive
```

Listens on port **9710**.

## Cargo.toml

```toml
[dependencies]
tidesdb = { version = "0.11", features = ["objectstore"] }
```

The `objectstore` feature enables the S3 connector and links against libcurl/openssl. On first build, cargo automatically downloads and compiles the TidesDB C library from source.

## Creating the bucket

Warpdrive requires requests to be SigV4-signed. Use the AWS CLI:

```bash
AWS_ACCESS_KEY_ID=adminkey \
AWS_SECRET_ACCESS_KEY=adminsecretkey123456 \
AWS_DEFAULT_REGION=us-east-1 \
  aws s3api create-bucket --bucket my-tidesdb-bucket \
    --endpoint-url http://localhost:9710
```

## Rust code

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

## Critical config flags

Both flags must be set explicitly — the defaults do not work with Warpdrive:

| Flag | Default | Required | Reason |
|------|---------|----------|--------|
| `use_path_style(true)` | `false` | Yes | Default is virtual-hosted style (`bucket.host/key`); Warpdrive uses path-style (`host/bucket/key`) |
| `use_ssl(false)` | `false` | Yes (already correct) | Warpdrive listens on plain HTTP |

Without `use_path_style(true)` TidesDB constructs URLs like `http://my-tidesdb-bucket.localhost:9710/UNIMAP` which Warpdrive cannot route, and all uploads silently fail.

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

## Working example

A self-contained runnable demo is in [`tidesdb-demo/`](../../tidesdb-demo/) at the repo root.

```bash
cd tidesdb-demo
PATH="$VENV/bin:$PATH" cargo run
```

It creates the bucket, writes 100 KV pairs, flushes them to Warpdrive, reads them back, and lists the objects stored in the bucket.
