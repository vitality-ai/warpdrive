# Developer's Guide

WarpDrive's default, recommended way to run is the **distributed cluster
engine** — erasure-coded, multi-node, and fully S3-compatible. Single-node
mode still exists (see [below](#single-node-mode-not-the-default)) but isn't
the onboarding path anymore.

**Both modes serve the exact same S3 URL** (`/s3/...`, or root — see
[4. Verify with boto3](#4-verify-with-boto3)). Which engine actually answers
it is decided once, at process startup, by whether the process was started
with `WARPDRIVE_PEERS` set: no peers means single-node, a peer list means
distributed. A client never needs to know or care which one it's talking
to.

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

That's the whole setup. It builds the release binary, then starts **5 local
nodes**, each in its own data directory under `server/cluster-local/`, wired
together as a cluster with `RS(3,2)` — WarpDrive's own default erasure code
(the same fallback `ec_params_from_env` uses if you set `WARPDRIVE_PEERS` by
hand without `WARPDRIVE_RS_K`/`WARPDRIVE_RS_M`). Every object is written as
3 data shards + 2 parity shards, one per node. Press Ctrl-C to stop every
node.

**Tolerates 2 node failures, for both reads and writes.** Write quorum here
is `k=3`, reachable with only 3 of the 5 nodes up — no asterisk, unlike the
smaller configs below.

To run a smaller local cluster instead, pass data-shard count and
parity-shard count explicitly — the smallest *redundant* config is `1 1`
(2 nodes, full mirroring):

```bash
./scripts/run_local_cluster.sh 1 1
```

**This one tolerates a node down for reads only, not writes**: write quorum
is `k+1` whenever `m == k` (MinIO's own rule for this edge case — see
`required_write_acks` in `coordinator.rs`), which for `RS(1,1)` is `2` —
i.e. *both* nodes. WarpDrive's distributed engine always requires at least
1 parity shard (`m >= 1`) to compute, so two mirrored nodes is the floor;
there's no "1-node cluster" mode — a single, unreplicated node is
single-node mode (below), not a degenerate cluster.

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
    endpoint_url='http://localhost:9710/s3',
    aws_access_key_id='adminkey',
    aws_secret_access_key='adminsecretkey123456',
    region_name='us-east-1',
    config=boto3.session.Config(s3={'addressing_style': 'path'}),
)

s3.put_object(Bucket='test-bucket', Key='hello.txt', Body=b'Hello, WarpDrive cluster!')
print(s3.get_object(Bucket='test-bucket', Key='hello.txt')['Body'].read())
```

Any node in the cluster can serve this — there's no dedicated coordinator,
so pointing `endpoint_url` at any node's port works the same. This is the
same `/s3/...` endpoint single-node mode uses, below — the client code
doesn't change between the two modes, only how the server was started
does. (`/cluster/s3/...` still works too, unchanged, kept for existing
tooling that already targets it explicitly.)

---

## Single-node mode (not the default)

The same binary, started without `WARPDRIVE_PEERS`, at the same `/s3/...`
URL as above — no separate mode to learn, just a different startup
decision. Not recommended for new setups (use the cluster above), but
still fully supported for a local scratch instance or anything that
genuinely doesn't need multi-node redundancy.

```bash
cd server
cargo build --release
WARPDRIVE_ADMIN_ACCESS_KEY=adminkey \
WARPDRIVE_ADMIN_SECRET_KEY=adminsecretkey123456 \
  ./target/release/warp_drive
```

Must be started from the `server/` directory (it looks for `server_log.yaml`
and needs writable `logs/` and `storage/` subdirectories). Verify with the
exact same boto3 snippet above (`endpoint_url='http://localhost:9710/s3'`) —
`create_bucket` also works here, since single-node mode has a real bucket
registry (the cluster engine doesn't, by design — see the User Guide).

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
