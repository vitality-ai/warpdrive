#!/usr/bin/env bash
# Author: Tejus Chandrashekar
# Launches an N-node WarpDrive cluster entirely on localhost, each node a
# separate process on its own port with its own storage/location logs.
# Usage: launch_local_cluster.sh <n_nodes> <base_port> <work_dir>
set -euo pipefail

N=${1:-5}
BASE_PORT=${2:-9710}
WORK_DIR=${3:-/tmp/wd-local-cluster}
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SERVER_DIR="$(cd "$SCRIPT_DIR/../server" && pwd)"
BIN="$SERVER_DIR/target/release/warp_drive"
LOG_CFG="$SERVER_DIR/server_log.yaml"

if [ ! -x "$BIN" ]; then
  echo "binary not found at $BIN -- build it first: (cd server && cargo build --release --bin warp_drive)" >&2
  exit 1
fi

mkdir -p "$WORK_DIR"
rm -f "$WORK_DIR"/*.pid

PEERS=""
for ((i=0; i<N; i++)); do
  PORT=$((BASE_PORT + i))
  PEERS="${PEERS}http://127.0.0.1:${PORT}"
  if [ $i -lt $((N-1)) ]; then PEERS="${PEERS},"; fi
done
echo "Peers: $PEERS"

for ((i=0; i<N; i++)); do
  PORT=$((BASE_PORT + i))
  NODE_DIR="$WORK_DIR/node$i"
  mkdir -p "$NODE_DIR/logs" "$NODE_DIR/storage"
  cp "$LOG_CFG" "$NODE_DIR/server_log.yaml"

  (
    cd "$NODE_DIR"
    WARPDRIVE_PORT=$PORT \
    WARPDRIVE_PEERS="$PEERS" \
    WARPDRIVE_RS_K=3 \
    WARPDRIVE_RS_M=2 \
    WARPDRIVE_LOCATION_LOG="$NODE_DIR/location.log" \
    WARPDRIVE_CONTENT_LOCATION_LOG="$NODE_DIR/content_location.log" \
    WARPDRIVE_BUCKET_CONFIG_LOG="$NODE_DIR/bucket_config.log" \
    WARPDRIVE_SHARD_META_LOG="$NODE_DIR/shard_meta.log" \
    STORAGE_DIRECTORY="$NODE_DIR/storage" \
    WARPDRIVE_ADMIN_ACCESS_KEY="AKIAIOSFODNN7EXAMPLE" \
    WARPDRIVE_ADMIN_SECRET_KEY="wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY" \
    RUST_LOG=warn \
    nohup "$BIN" > "$NODE_DIR/stdout.log" 2>&1 &
    echo $! > "$WORK_DIR/node$i.pid"
  )
  echo "Started node$i on port $PORT (pid $(cat "$WORK_DIR/node$i.pid"))"
done

echo "Waiting for all nodes to become healthy..."
for ((i=0; i<N; i++)); do
  PORT=$((BASE_PORT + i))
  NODE_DIR="$WORK_DIR/node$i"
  for attempt in $(seq 1 30); do
    if curl -sf "http://127.0.0.1:${PORT}/cluster/_internal/timing" >/dev/null 2>&1; then
      break
    fi
    sleep 0.5
    if [ "$attempt" -eq 30 ]; then
      echo "node$i on port $PORT did not become healthy" >&2
      cat "$NODE_DIR/stdout.log" >&2 || true
      exit 1
    fi
  done
done
echo "All $N nodes healthy. PEERS=$PEERS"
