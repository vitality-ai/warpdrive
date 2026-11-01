# Developer's Guide

WarpDrive's default, recommended way to run is the **distributed cluster engine**
— erasure-coded, multi-node, and still fully S3-compatible. Single-node mode
still exists (see [below](#single-node-mode-not-the-default)) but isn't the
onboarding path anymore.

## Local Setup

---

### 1. Prerequisites

- **Rust** — [install via rustup](https://rustup.rs/):
  ```bash
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
  ```

- **libsqlite3-dev** and **FlatBuffers compiler (`flatc`)**

  Ubuntu/Debian:
  ```bash
  sudo apt-get update && sudo apt-get install -y libsqlite3-dev flatbuffers-compiler
  ```

  macOS (Homebrew):
  ```bash
  brew install sqlite3 flatbuffers
  ```

---

### 2. Clone the Repository

```bash
git clone https://github.com/vitality-ai/warpdrive.git
cd warpdrive
```

---

### 3. Run a cluster

```bash
./scripts/run_local_cluster.sh
```

That's the whole setup. It builds the release binary, then starts **2 local
nodes**, each in its own data directory under `server/cluster-local/`, wired
together as a 2-node cluster with `RS(1,1)` — every object is written as one
data shard + one parity shard, one per node, so the data is fully mirrored
and the cluster keeps serving reads and writes if either node goes down.
Press Ctrl-C to stop both nodes.

This is the smallest real distributed-mode config: WarpDrive's erasure coding
requires at least 1 parity shard (`m >= 1`), so a lone, unreplicated node
isn't a supported "cluster of one" — two mirrored nodes is the floor, and
it's a genuinely redundant deployment, not a toy.

To run more nodes with a different code, pass data-shard count and
parity-shard count explicitly, e.g. a 5-node cluster matching the project's
own default (`RS(3,2)`, 3 data + 2 parity, tolerates 2 node failures):

```bash
./scripts/run_local_cluster.sh 3 2
```

Each node listens on its own port starting at `9710` (`9710`, `9711`, ...).
Credentials default to `adminkey` / `adminsecretkey123456` for SigV4 auth —
override with `WARPDRIVE_ADMIN_ACCESS_KEY`/`WARPDRIVE_ADMIN_SECRET_KEY` env
vars before running the script if you want your own.

#### Running nodes on separate machines

The script is a convenience for one machine. To run a real multi-host
cluster, start the same binary on each host instead, passing peer addresses
directly — see [Distributed Mode](../docs/user_guide.md#distributed-mode-default)
in the User Guide for every node, join, and transport env var, including
how to make a node's own address (`WARPDRIVE_SELF_ADDR`) and the port it
binds (`WARPDRIVE_PORT`) configurable per host.

---

### 4. Verify with boto3

```bash
pip install boto3
```

```python
import boto3

s3 = boto3.client(
    's3',
    endpoint_url='http://localhost:9710/cluster/s3',
    aws_access_key_id='adminkey',
    aws_secret_access_key='adminsecretkey123456',
    region_name='us-east-1',
    config=boto3.session.Config(s3={'addressing_style': 'path'}),
)

s3.put_object(Bucket='test-bucket', Key='hello.txt', Body=b'Hello, WarpDrive cluster!')
print(s3.get_object(Bucket='test-bucket', Key='hello.txt')['Body'].read())
```

Any node in the cluster can serve this — there's no dedicated coordinator,
so pointing `endpoint_url` at any node's port works the same.

---

## Single-node mode (not the default)

A single, non-distributed process is still there for cases that don't need
erasure coding or multi-node fault tolerance (e.g. a quick local scratch
instance). It isn't the recommended path anymore — use the cluster above
unless you specifically want this.

```bash
cd server
cargo build --release
WARPDRIVE_ADMIN_ACCESS_KEY=adminkey \
WARPDRIVE_ADMIN_SECRET_KEY=adminsecretkey123456 \
  ./target/release/warp_drive
```

Must be started from the `server/` directory (it looks for `server_log.yaml`
and needs writable `logs/` and `storage/` subdirectories). Listens on port
9710 (S3 API under `/s3/...`), same credential env vars as the cluster.

```python
import boto3

s3 = boto3.client(
    's3',
    endpoint_url='http://localhost:9710',
    aws_access_key_id='adminkey',
    aws_secret_access_key='adminsecretkey123456',
    region_name='us-east-1',
    config=boto3.session.Config(s3={'addressing_style': 'path'}),
)

s3.create_bucket(Bucket='test-bucket')
s3.put_object(Bucket='test-bucket', Key='hello.txt', Body=b'Hello, Warpdrive!')
print(s3.get_object(Bucket='test-bucket', Key='hello.txt')['Body'].read())
```

---

## Docker (Optional)

The published image runs single-node mode only; there's no multi-container
cluster compose setup yet.

**Build:**
```bash
docker build -t warpdrive .
```

**Run:**
```bash
docker run -p 9710:9710 \
  -e WARPDRIVE_ADMIN_ACCESS_KEY=adminkey \
  -e WARPDRIVE_ADMIN_SECRET_KEY=adminsecretkey123456 \
  warpdrive
```

---

## Troubleshooting

- Ensure all prerequisites are installed and available in your PATH.
- For advanced help, see [Rust docs](https://doc.rust-lang.org/book/) or [FlatBuffers docs](https://google.github.io/flatbuffers/).
