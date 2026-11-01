#!/usr/bin/env bash
# Runs a local WarpDrive cluster inside ONE container: several warp_drive
# processes sharing this container's network namespace (so they reach
# each other over plain 127.0.0.1, no Docker DNS/multiple containers
# needed), each with its own data directory under /data so one `-v`
# mount captures every node's state. The in-container equivalent of
# scripts/run_local_cluster.sh (no `cargo build` here -- the Dockerfile
# already built the release binary).
#
# Usage: docker run -p 9710-9714:9710-9714 -v ./data:/data <image> \
#          /server/docker_cluster_entrypoint.sh [K] [M]
set -euo pipefail

K="${1:-3}"
M="${2:-2}"
if [ "$M" -lt 1 ]; then
  echo "M must be >= 1 -- the distributed engine is always erasure-coded." >&2
  exit 1
fi
NODES=$((K + M))
BASE_PORT="${WARPDRIVE_BASE_PORT:-9710}"
ACCESS_KEY="${WARPDRIVE_ADMIN_ACCESS_KEY:-adminkey}"
SECRET_KEY="${WARPDRIVE_ADMIN_SECRET_KEY:-adminsecretkey123456}"
BIN="/server/target/release/warp_drive"

PEERS=""
for i in $(seq 0 $((NODES - 1))); do
  port=$((BASE_PORT + i))
  PEERS="${PEERS}http://127.0.0.1:${port},"
done
PEERS="${PEERS%,}"

echo "Starting $NODES node(s) in this one container, RS($K,$M), peers: $PEERS"

PIDS=()
cleanup() {
  echo "Stopping cluster..."
  for pid in "${PIDS[@]}"; do
    kill "$pid" 2>/dev/null || true
  done
  wait 2>/dev/null || true
}
trap cleanup INT TERM

for i in $(seq 0 $((NODES - 1))); do
  port=$((BASE_PORT + i))
  node_dir="/data/node$i"
  mkdir -p "$node_dir"
  cp /server/server_log.yaml "$node_dir/server_log.yaml"

  (
    cd "$node_dir"
    WARPDRIVE_PORT="$port" \
    WARPDRIVE_SELF_ADDR="http://127.0.0.1:${port}" \
    WARPDRIVE_PEERS="$PEERS" \
    WARPDRIVE_RS_K="$K" \
    WARPDRIVE_RS_M="$M" \
    WARPDRIVE_ADMIN_ACCESS_KEY="$ACCESS_KEY" \
    WARPDRIVE_ADMIN_SECRET_KEY="$SECRET_KEY" \
    STORAGE_DIRECTORY="$node_dir/storage" \
    WARPDRIVE_LOCATION_LOG="$node_dir/cluster_location.log" \
    WARPDRIVE_CONTENT_LOCATION_LOG="$node_dir/cluster_content_location.log" \
    WARPDRIVE_BUCKET_CONFIG_LOG="$node_dir/cluster_bucket_config.log" \
    WARPDRIVE_SHARD_META_LOG="$node_dir/shard_meta.log" \
    exec "$BIN" > "$node_dir/node.out" 2>&1
  ) &
  PIDS+=($!)
  echo "  node$i: pid $!, port $port, data: $node_dir"
done

echo "Cluster is up. S3 endpoint on any node, e.g. http://localhost:${BASE_PORT}/s3/"
wait
