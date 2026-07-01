# Using SlateDB with Warpdrive

> **Note:** This guide was written and validated against a locally built and locally running Warpdrive instance. Warpdrive was compiled from source (`cargo build --release`), started on `localhost:9710`, and used as the S3 backend for SlateDB — no cloud account or external service required.

SlateDB is an embedded LSM-tree key-value storage engine built entirely on object storage. Every write is persisted as an SSTable file uploaded to an S3-compatible backend. Warpdrive works as that backend out of the box.

## How it works

```
Your Application
      │
      ▼
   SlateDB  (embedded, in-process)
      │
      │  S3 API  (HTTP, via object_store crate)
      ▼
   Warpdrive  (localhost:9710)
```

When SlateDB opens it:
1. Buffers writes in an in-memory memtable
2. On flush, serializes the memtable into SSTable files and uploads them to Warpdrive as S3 objects
3. Maintains a manifest in the bucket tracking which SSTables exist at each level
4. Runs background compaction, uploading compacted SSTables and deleting obsolete ones
5. On restart, downloads the latest manifest from Warpdrive and rebuilds local state from there

## Prerequisites

**Warpdrive running:**

```bash
cd warpdrive/server
WARPDRIVE_ADMIN_ACCESS_KEY=adminkey \
WARPDRIVE_ADMIN_SECRET_KEY=adminsecretkey123456 \
  ./target/release/warp_drive
```

Listens on port **9710**.

In `Cargo.toml`:

```toml
[dependencies]
slatedb    = { version = "0.14", features = ["aws"] }
object_store = { version = "0.14", features = ["aws"] }
tokio      = { version = "1", features = ["full"] }
anyhow     = "1"
```

## Creating the bucket

```bash
AWS_ACCESS_KEY_ID=adminkey \
AWS_SECRET_ACCESS_KEY=adminsecretkey123456 \
AWS_DEFAULT_REGION=us-east-1 \
  aws s3api create-bucket --bucket my-slatedb-bucket \
    --endpoint-url http://localhost:9710
```

## Example

A runnable version of this demo is in [`demo/slatedb/`](../../demo/slatedb) in this repository.

```rust
use object_store::aws::AmazonS3Builder;
use slatedb::Db;
use std::sync::Arc;

let object_store = Arc::new(
    AmazonS3Builder::new()
        .with_allow_http(true)                     // plain HTTP — no TLS needed
        .with_endpoint("http://localhost:9710")
        .with_access_key_id("adminkey")
        .with_secret_access_key("adminsecretkey123456")
        .with_bucket_name("my-slatedb-bucket")
        .with_region("us-east-1")
        .build()?,
);

// Local path is used as a read cache; durable state lives in Warpdrive
let db = Db::open("/tmp/slatedb-local-cache", object_store).await?;

// Write
db.put(b"hello", b"world").await?;

// Flush to Warpdrive (uploads WAL SSTable + manifest)
db.flush().await?;

// Read
let val = db.get(b"hello").await?;
println!("{}", String::from_utf8_lossy(&val.unwrap())); // "world"

// Range scan
let mut iter = db.scan(b"key-a"..=b"key-z").await?;
while let Ok(Some(kv)) = iter.next().await {
    println!("{} => {}", String::from_utf8_lossy(&kv.key), String::from_utf8_lossy(&kv.value));
}

// Delete
db.delete(b"hello").await?;
db.flush().await?;

db.close().await?;
```

To run the full demo (creates bucket, writes 1000 keys, flushes, spot-checks reads, range scans, deletes one key):

```bash
cd demo/slatedb
cargo run --release
```

## Critical config flags

| Flag | Required | Reason |
|------|----------|--------|
| `with_allow_http(true)` | Yes | Warpdrive listens on plain HTTP; `object_store` defaults to requiring HTTPS |
| `with_endpoint(...)` | Yes | Must point at `http://localhost:9710`; omitting this sends requests to AWS S3 |

## What gets stored in Warpdrive

After a flush the bucket will contain objects under the local cache path you passed to `Db::open`:

| Object key pattern | Description |
|--------------------|-------------|
| `<cache>/wal/XXXXXXXXXXXXXXXXXXXXXXXX.sst` | WAL SSTable segments — written continuously as keys arrive |
| `<cache>/manifest/XXXXXXXXXXXXXXXXXXXXXXXX.manifest` | Manifest snapshots — updated on every flush and compaction |
| `<cache>/compacted/XXXXXXXXXXXXXXXXXXXXXXXX.sst` | Compacted SSTables — produced by background compaction |
| `<cache>/compactions/XXXXXXXXXXXXXXXXXXXXXXXX.compactions` | Compaction metadata |

List them at any time:

```bash
AWS_ACCESS_KEY_ID=adminkey \
AWS_SECRET_ACCESS_KEY=adminsecretkey123456 \
AWS_DEFAULT_REGION=us-east-1 \
  aws s3api list-objects-v2 --bucket my-slatedb-bucket \
    --endpoint-url http://localhost:9710
```

## Recovery

SlateDB recovers entirely from the bucket. Delete the local cache directory and reopen with the same `object_store` config — SlateDB will download the latest manifest from Warpdrive and resume from where it left off. The local path is a read cache only; nothing is lost if it disappears.

## ISO 8601 date requirement

`object_store` 0.14 (the crate SlateDB uses internally) **requires ISO 8601 timestamps** (`2026-07-01T05:14:19.000Z`) in ListObjectsV2 XML responses. It does not accept the RFC 2616 format (`Wed, 01 Jul 2026 05:14:19 GMT`) that some S3-compatible servers emit for `LastModified`.

Warpdrive stores and returns ISO 8601 for all `LastModified` fields in XML bodies. If you are running an older Warpdrive build that emits RFC 2616 dates, SlateDB's manifest poller will silently fail to parse the object list and enter an infinite retry loop — no error is surfaced. Upgrade to the current build to resolve this.

## Demo results

The demo in `demo/slatedb/` exercises:

- **Write** — 1000 sequential keys (`key-0000` … `key-0999`)
- **Flush** — uploads WAL SSTables and manifests to Warpdrive
- **Spot-check reads** — retrieves `key-0000`, `key-0042`, `key-0999`
- **Range scan** — iterates `key-0010 ..= key-0012`
- **Delete** — deletes `key-0042`, flushes, confirms `get` returns `None`
- **Close** — clean shutdown, background compaction threads join

All operations complete successfully against a locally running Warpdrive instance.
