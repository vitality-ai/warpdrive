#!/usr/bin/env bash
# Start a K+M node WarpDrive cluster on this host, one process per node, and
# wait until every node answers HTTP. Used by the S3 compatibility job so the
# suite runs against the distributed engine.
#
# Usage: start_cluster.sh <warp_drive binary> <run dir>
# Env:   WARPDRIVE_ADMIN_ACCESS_KEY, WARPDRIVE_ADMIN_SECRET_KEY (required)
#        RS_K (default 3), RS_M (default 2), BASE_PORT (default 9710)
#
# Node i listens on BASE_PORT+i for HTTP, +1000 for gRPC and +2000 for raw
# TCP, so a 5-node cluster on 9710 uses 9710-9714, 10710-10714, 11710-11714.
# Node 0 (BASE_PORT) is the S3 endpoint the suite talks to.
set -euo pipefail

BIN="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
RUN_DIR="$2"
K="${RS_K:-3}"
M="${RS_M:-2}"
BASE_PORT="${BASE_PORT:-9710}"
NODES=$((K + M))
SERVER_LOG_YAML="$(cd "$(dirname "$0")/../../server" && pwd)/server_log.yaml"
: "${WARPDRIVE_ADMIN_ACCESS_KEY:?must be set}"
: "${WARPDRIVE_ADMIN_SECRET_KEY:?must be set}"
# Required once cluster endpoints need a shared secret. Harmless before that.
export WARPDRIVE_CLUSTER_SECRET="${WARPDRIVE_CLUSTER_SECRET:-ci-cluster-secret-0123456789}"

PEERS=""
for i in $(seq 0 $((NODES - 1))); do
  PEERS="${PEERS}http://127.0.0.1:$((BASE_PORT + i)),"
done
PEERS="${PEERS%,}"

mkdir -p "$RUN_DIR"
for i in $(seq 0 $((NODES - 1))); do
  port=$((BASE_PORT + i))
  node_dir="$RUN_DIR/node$i"
  mkdir -p "$node_dir"
  # warp_drive reads server_log.yaml from its working directory.
  cp "$SERVER_LOG_YAML" "$node_dir/"
  (
    cd "$node_dir"
    WARPDRIVE_PORT="$port" \
    WARPDRIVE_SELF_ADDR="http://127.0.0.1:$port" \
    WARPDRIVE_PEERS="$PEERS" \
    WARPDRIVE_RS_K="$K" \
    WARPDRIVE_RS_M="$M" \
      nohup "$BIN" > node.out 2>&1 &
    echo $! > pid
  )
done

for i in $(seq 0 $((NODES - 1))); do
  port=$((BASE_PORT + i))
  for _ in $(seq 1 30); do
    # Any HTTP response means the node is up.
    if curl -s -o /dev/null "http://127.0.0.1:$port/"; then
      continue 2
    fi
    sleep 1
  done
  echo "node$i (port $port) did not come up in time"
  cat "$RUN_DIR/node$i/node.out"
  exit 1
done
echo "RS($K,$M) cluster up: $PEERS"
