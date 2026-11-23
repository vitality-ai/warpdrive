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

Set `WARPDRIVE_SELF_ADDR` on every node, to its own entry in
`WARPDRIVE_PEERS`. Each node finds its slot in the peer list from it (the
slot keeps the object addresses nodes hand out from colliding). Without it,
a node falls back to the one peer whose port matches its own
`WARPDRIVE_PORT`, which only works when every node uses a different port.
With several hosts on the same port, a node won't start without it (or an
explicit `WARPDRIVE_NODE_SLOT`, 0-255).

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

## Prebuilt binaries

Each [GitHub Release](https://github.com/vitality-ai/warpdrive/releases)
has `warp_drive` archives for Linux amd64, Linux arm64 and macOS Apple
Silicon. SQLite is compiled in, so the only runtime dependency is libc.
Linux binaries need glibc 2.35+ (Ubuntu 22.04+, Debian 12+).

```bash
TAG=v1.0.0-beta
PLATFORM=darwin-arm64   # or linux-amd64, linux-arm64
curl -fLO https://github.com/vitality-ai/warpdrive/releases/download/$TAG/warp_drive-$TAG-$PLATFORM.tar.gz
tar -xzf warp_drive-$TAG-$PLATFORM.tar.gz
cd warp_drive-$TAG-$PLATFORM
WARPDRIVE_ADMIN_ACCESS_KEY=adminkey \
WARPDRIVE_ADMIN_SECRET_KEY=adminsecretkey123456 \
  ./warp_drive
```

Start it from the extracted folder. It reads `server_log.yaml` from its
working directory and keeps its data there too.

On macOS, a binary downloaded through a browser is quarantined because it
isn't notarized. Clear that with `xattr -d com.apple.quarantine warp_drive`
before running it. Downloads with `curl` or `gh release download` aren't
quarantined.

---

## Docker (Optional)

**Pull a pre-built image** instead of building from source. It's rebuilt
and pushed on every merge to `main`, for both `linux/amd64` and
`linux/arm64`. Docker picks the right one for your machine:

```bash
docker pull ghcr.io/vitality-ai/warpdrive:latest

docker run -p 9710:9710 \
  -e WARPDRIVE_ADMIN_ACCESS_KEY=adminkey \
  -e WARPDRIVE_ADMIN_SECRET_KEY=adminsecretkey123456 \
  ghcr.io/vitality-ai/warpdrive:latest
```

The same image is also on Docker Hub as `tejascj/warpdrive:latest`, if
your network can't reach `ghcr.io`.

**Or build from source** (`docker build`/`docker run` directly, or `docker
compose up` — both give you the same single-node default, consistent with
the apt package's own default: a lone instance has no peers to cluster
with):

```bash
docker build -t warpdrive .
docker run -p 9710:9710 \
  -e WARPDRIVE_ADMIN_ACCESS_KEY=adminkey \
  -e WARPDRIVE_ADMIN_SECRET_KEY=adminsecretkey123456 \
  warpdrive
```

**A local cluster, in one container.** Several `warp_drive` processes
sharing that one container's network namespace (so they reach each other
over plain `127.0.0.1`, no multiple containers or Docker DNS needed),
each with its own data directory under `/data` so a single volume mount
captures every node's state:

```bash
docker build -t warpdrive .
docker run -p 9710-9714:9710-9714 -v "$(pwd)/data:/data" warpdrive \
  /server/docker_cluster_entrypoint.sh
```

Defaults to `RS(3,2)` (5 processes, ports 9710-9714) — pass `K M` as
extra args to the entrypoint for a different code, e.g. `... warpdrive
/server/docker_cluster_entrypoint.sh 1 1` for a 2-process mirror. Verify
against `http://localhost:9710/s3` same as single node — any node's port
works, there's no dedicated coordinator.

This is a local dev/demo convenience, same as `scripts/run_local_cluster.sh`
on bare metal — not a production deployment story. For running a real
cluster across multiple machines, see
[Production Deployment](Production-Deployment.md).

---

## Logs

`warp_drive` reads `server_log.yaml` from its working directory. The
default level is `info`, which logs startup, cluster and bucket changes,
and background job summaries, never anything per request.

| Level | What it logs |
|---|---|
| `error` | Something failed and needs attention |
| `warn` | Degraded but still serving: a peer down, a failed replication |
| `info` (default) | Startup and configuration, nodes joining the cluster, bucket create/delete and config changes, background job summaries (e.g. space freed by the deletion worker) |
| `debug` | Diagnostic detail that doesn't grow with traffic: resolved config, credential cache refreshes, STS sessions issued |
| `trace` | Everything per request: each S3 call, rejected requests (bad signature, missing key), step-by-step internals |

Every message goes to two places: the console (stdout) and
`logs/application.log` under the working directory. The file rotates at
100 MB and keeps 5 old files (`application.1.log` to `application.5.log`).

### Reading logs

**`.deb` package (systemd)**

```bash
journalctl -u warpdrive -f                                   # follow
journalctl -u warpdrive --since "1 hour ago"
sudo tail -f /var/lib/warpdrive/logs/application.log
```

**Release archive**

The console is the terminal you started it from. The file is
`logs/application.log` inside the extracted folder:

```bash
tail -f logs/application.log
```

**Docker image**

```bash
docker logs -f <container>
docker exec <container> tail -f /server/logs/application.log
docker cp <container>:/server/logs ./warpdrive-logs           # copy out, rotated files included
```

The log file sits in the container's own filesystem, not on the `/data`
volume, so it's lost when the container is removed. `docker logs` has the
same messages.

**Docker cluster entrypoint** (`docker_cluster_entrypoint.sh`)

Each node writes under its own directory on the `/data` mount:

```bash
tail -f data/node0/logs/application.log    # node 0, from the host
tail -f data/node0/node.out                # node 0's console output
```

**Kubernetes** (see [Production Deployment](Production-Deployment.md))

```bash
kubectl logs -f warpdrive-0
kubectl logs warpdrive-0 --previous        # the container before its last restart
kubectl exec warpdrive-0 -- tail -f /server/logs/application.log
```

### More detail while debugging

In `server_log.yaml`, change the `warp_drive` logger at the bottom to `debug`,
or to `trace` to see every request:

```yaml
loggers:
  warp_drive:
    level: debug
```

Change only that logger, not `root`. At `debug`, `root` also turns on the
HTTP and gRPC libraries, which log every network frame and slow the
server down several times over. The file is re-read every 30 seconds, so a
running server picks up the change without a restart. Set it back to
`info` when you're done, because `trace` writes several lines for every
request.

| Install | `server_log.yaml` |
|---|---|
| `.deb` | `/var/lib/warpdrive/server_log.yaml` |
| Release archive | the extracted folder |
| Docker image | `/server/server_log.yaml`. Edit with `docker exec`, or mount your own: `-v ./server_log.yaml:/server/server_log.yaml` |
| Docker cluster entrypoint | `data/node<N>/server_log.yaml` (one per node) |

The per-request access log is off unless the server is started with
`WARPDRIVE_ACCESS_LOG=1`. It logs at `info`, so it shows at the default
level once enabled.

### Files that are not logs

`cluster_location.log`, `cluster_content_location.log`,
`cluster_bucket_config.log` and `shard_meta.log` hold cluster metadata
(where each object's shards live). Don't delete or rotate them. They're
how a node finds the objects it stores.

---

## Troubleshooting

- Ensure all prerequisites are installed and available in your PATH.
- For advanced help, see [Rust docs](https://doc.rust-lang.org/book/) or [FlatBuffers docs](https://google.github.io/flatbuffers/).
