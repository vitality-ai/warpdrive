# Using Neon with Warpdrive

> **Note:** This guide was written and validated against a locally built and locally running Warpdrive instance. Warpdrive was compiled from source (`cargo build --release`), started on `localhost:9710`, and used as the S3 backend for a locally running Neon cluster — no cloud account or external service required.

Neon is an open-source serverless Postgres platform that separates storage from compute. It stores all durable state — WAL segments, page layer files, and manifests — in an S3-compatible object store. Warpdrive works as that backend out of the box.

## How it works

```
psql / application
        │
        │  PostgreSQL wire protocol
        ▼
   Neon Compute  (patched Postgres, stateless)
        │
        │  Neon storage protocol
        ▼
   Pageserver  (page storage, cache, compaction)
   Safekeeper  (WAL durability, quorum writes)
        │
        │  S3 API  (HTTP, aws-sdk-rust, path-style)
        ▼
   Warpdrive  (localhost:9710)
```

Neon's storage layer consists of three services:

- **Pageserver** — accepts page reads/writes from compute nodes, buffers them in memory, and periodically flushes page layer files (SSTables) to S3. On restart it rehydrates from S3.
- **Safekeeper** — receives WAL from Postgres, forms a quorum across multiple safekeepers for durability, and archives WAL segments to S3.
- **Storage broker** — coordinates pageserver and safekeeper discovery; does not use S3 directly.

## S3 API operations used

Neon's `remote_storage` library (`libs/remote_storage/src/s3_bucket.rs`) uses the following S3 operations — all supported by Warpdrive:

| Operation | Used for |
|-----------|---------|
| `PutObject` | Uploading layer files, WAL segments, manifests, tenant manifests |
| `GetObject` | Downloading layers on cache miss; optional `Range` header for partial reads |
| `HeadObject` | Probing object existence; response must include `Last-Modified` and `Content-Length` |
| `ListObjectsV2` | Discovering which layer files exist for a tenant/timeline |
| `DeleteObjects` | Bulk deletion during compaction (up to 1000 keys per request) |
| `CopyObject` | Time-travel recovery (restoring an older version of a layer) |
| `ListObjectVersions` | Time-travel recovery (enumerating all historical versions) |

No multipart upload is used — every object is a single `PutObject` with `Content-Length` set.

## What gets stored in Warpdrive

```
neon/
├── pageserver/
│   └── tenants/<tenant-id>/
│       ├── tenant-manifest-XXXXXXXX.json          # tenant metadata
│       └── timelines/<timeline-id>/
│           ├── initdb.tar.zst                     # initial Postgres cluster archive
│           ├── index_part.json-XXXXXXXX           # layer index for this timeline
│           └── <lsn-range>__<lsn>-<lsn>-XXXXXXXX # page layer files (delta + image)
└── safekeeper/
    └── <tenant-id>/<timeline-id>/
        └── <lsn>.partial                          # WAL segments
```

Layer filenames encode the key range and LSN range they cover. Warpdrive stores them as regular S3 objects.

## Prerequisites

### Build dependencies (Ubuntu/Debian)

```bash
sudo apt-get install -y \
  build-essential libtool libreadline-dev flex bison libseccomp-dev \
  libssl-dev clang pkg-config libpq-dev cmake postgresql-client \
  libprotobuf-dev libcurl4-openssl-dev openssl lsof libicu-dev
```

`protoc` 3.15+ is required. Ubuntu 22.04 ships 3.12, so install a newer binary:

```bash
curl -sL https://github.com/protocolbuffers/protobuf/releases/download/v25.3/protoc-25.3-linux-x86_64.zip \
  -o /tmp/protoc.zip
unzip -q /tmp/protoc.zip -d /tmp/protoc-install
cp /tmp/protoc-install/bin/protoc ~/.local/bin/
cp -r /tmp/protoc-install/include/* ~/.local/include/
```

### Build Neon from source

```bash
git clone --depth=1 https://github.com/neondatabase/neon.git
cd neon
git submodule update --init --depth=1 \
  vendor/postgres-v14 vendor/postgres-v15 \
  vendor/postgres-v16 vendor/postgres-v17

# Install the Rust toolchain pinned in rust-toolchain.toml
rustup toolchain install "$(grep channel rust-toolchain.toml | cut -d'"' -f2)" \
  --profile minimal --component rustfmt --component clippy

PATH="$HOME/.local/bin:$PATH" PROTOC="$HOME/.local/bin/protoc" \
  make -j4 -s BUILD_TYPE=release
```

The build produces binaries under `target/release/` and Postgres installations under `pg_install/v14` through `pg_install/v17`. All four PG versions must be built — the Rust workspace hard-codes bindings generation for all of them.

**Build time:** ~60 minutes on 8 cores.

### Warpdrive running

```bash
cd warpdrive/server
WARPDRIVE_ADMIN_ACCESS_KEY=adminkey \
WARPDRIVE_ADMIN_SECRET_KEY=adminsecretkey123456 \
  ./target/release/warp_drive
```

Listens on port **9710**.

## Configuring Neon to use Warpdrive

### 1. Create the bucket

```bash
AWS_ACCESS_KEY_ID=adminkey \
AWS_SECRET_ACCESS_KEY=adminsecretkey123456 \
AWS_DEFAULT_REGION=us-east-1 \
  aws s3api create-bucket --bucket neon --endpoint-url http://localhost:9710
```

### 2. Initialize the local cluster

```bash
cd neon
PATH="$PWD/target/release:$PWD/pg_install/v17/bin:$PATH" neon_local init
```

### 3. Point pageserver at Warpdrive

Edit `.neon/pageserver_1/pageserver.toml` — replace the default `local_path` remote storage with:

```toml
remote_storage = {endpoint='http://localhost:9710', bucket_name='neon', bucket_region='us-east-1', prefix_in_bucket='/pageserver'}
```

### 4. Point safekeeper at Warpdrive

Add `remote_storage` to the `[[safekeepers]]` block in `.neon/config`:

```toml
[[safekeepers]]
id = 1
pg_port = 5454
http_port = 7676
sync = true
auth_enabled = false
remote_storage = "{endpoint='http://localhost:9710', bucket_name='neon', bucket_region='us-east-1', prefix_in_bucket='/safekeeper/'}"
```

### 5. Start the cluster

Pass AWS credentials as environment variables — Neon's `remote_storage` library reads them from the standard `DefaultCredentialsChain`.

```bash
AWS_ACCESS_KEY_ID=adminkey \
AWS_SECRET_ACCESS_KEY=adminsecretkey123456 \
PATH="$PWD/target/release:$PWD/pg_install/v17/bin:$PATH" \
  neon_local start
```

### 6. Create a tenant and endpoint

```bash
AWS_ACCESS_KEY_ID=adminkey AWS_SECRET_ACCESS_KEY=adminsecretkey123456 \
  neon_local tenant create --set-default

neon_local endpoint create main --pg-version 17
neon_local endpoint start main
```

Postgres is now available at `postgresql://cloud_admin@127.0.0.1:55433/postgres`.

## Verifying data lands in Warpdrive

```bash
# Write some data
psql "postgresql://cloud_admin@127.0.0.1:55433/postgres" -c "
  CREATE TABLE test (id serial, val text);
  INSERT INTO test (val) VALUES ('hello'), ('world');
  SELECT * FROM test;
"

# List objects in Warpdrive
AWS_ACCESS_KEY_ID=adminkey \
AWS_SECRET_ACCESS_KEY=adminsecretkey123456 \
AWS_DEFAULT_REGION=us-east-1 \
  aws s3api list-objects-v2 --bucket neon \
    --endpoint-url http://localhost:9710 \
    --query 'Contents[].{Key:Key,Size:Size}' \
    --output table
```

After a checkpoint or WAL flush you will see:

```
pageserver/tenants/<id>/tenant-manifest-00000001.json        (~57 B)
pageserver/tenants/<id>/timelines/<id>/initdb.tar.zst        (~1.5 MB)
pageserver/tenants/<id>/timelines/<id>/index_part.json-...   (~400 B)
pageserver/tenants/<id>/timelines/<id>/<lsn-range>           (~23 MB, image layer)
```

## Recovery

Neon recovers fully from the bucket on restart. Stop all services, delete `.neon/`, re-run `neon_local init` + `neon_local start` with the same bucket config — the pageserver downloads the tenant manifest and layer index from Warpdrive and resumes serving the same data.

## Object accumulation over time

As Postgres receives writes, Neon accumulates objects in Warpdrive automatically:

- Every WAL flush produces a new segment in the safekeeper prefix
- The pageserver compacts delta layers into image layers, uploading new files and bulk-deleting old ones via `DeleteObjects`
- `index_part.json` is rewritten on every layer flush to track the current file set

All of these — `PutObject`, `GetObject`, `ListObjectsV2`, `DeleteObjects` — were verified working against Warpdrive in this integration.
