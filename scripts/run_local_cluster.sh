#!/usr/bin/env bash
# Bring up a local WarpDrive cluster on one machine: one process per node,
# each in its own directory so storage/logs/location-logs never collide.
#
# Usage: ./scripts/run_local_cluster.sh [K] [M]
#   K = data shards, M = parity shards (RS(K,M), see server/src/main.rs).
#   Node count = K+M. Default is K=3 M=2 -- the project's own default
#   erasure code (ec_params_from_env's fallback when WARPDRIVE_RS_K/M
#   aren't set), so this script's default matches what you get from just
#   setting WARPDRIVE_PEERS by hand. Tolerates 2 node failures for both
#   reads and writes (write quorum is k=3, reachable with only 3 of 5
#   nodes up). Pass `1 1` for the smaller 2-node mirror instead -- it
#   tolerates a node down for reads only, not writes (write quorum is
#   k+1 when m==k, so RS(1,1) needs both nodes up for a PUT/DELETE; see
#   required_write_acks in coordinator.rs). The distributed engine is
#   always erasure-coded (M must be >= 1) -- for a single node with no
#   redundancy, use single-node mode (`cd server && cargo run`, the plain
#   `/s3/...` API) instead of asking this script for a 1-node cluster.
#
# Ctrl-C stops every node.
set -euo pipefail

K="${1:-3}"
M="${2:-2}"
if [ "$M" -lt 1 ]; then
  echo "M must be >= 1 -- the distributed engine is always erasure-coded." >&2
  echo "For a single node with no redundancy, run single-node mode instead (cd server && cargo run)." >&2
  exit 1
fi
NODES=$((K + M))
BASE_PORT="${WARPDRIVE_BASE_PORT:-9710}"
ACCESS_KEY="${WARPDRIVE_ADMIN_ACCESS_KEY:-adminkey}"
SECRET_KEY="${WARPDRIVE_ADMIN_SECRET_KEY:-adminsecretkey123456}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SERVER_DIR="$SCRIPT_DIR/../server"
RUN_DIR="$SERVER_DIR/cluster-local"

echo "Building release binary (first run only takes a while)..."
(cd "$SERVER_DIR" && cargo build --release --quiet)
BIN="$SERVER_DIR/target/release/warp_drive"

PEERS=""
for i in $(seq 0 $((NODES - 1))); do
  port=$((BASE_PORT + i))
  PEERS="${PEERS}http://127.0.0.1:${port},"
done
PEERS="${PEERS%,}"

echo "Starting $NODES node(s), RS($K,$M), peers: $PEERS"

PIDS=()
cleanup() {
  echo
  echo "Stopping cluster..."
  for pid in "${PIDS[@]}"; do
    kill "$pid" 2>/dev/null || true
  done
  wait 2>/dev/null || true
}
trap cleanup INT TERM

for i in $(seq 0 $((NODES - 1))); do
  port=$((BASE_PORT + i))
  node_dir="$RUN_DIR/node$i"
  mkdir -p "$node_dir"
  cp "$SERVER_DIR/server_log.yaml" "$node_dir/server_log.yaml"

  (
    cd "$node_dir"
    WARPDRIVE_PORT="$port" \
    WARPDRIVE_SELF_ADDR="http://127.0.0.1:${port}" \
    WARPDRIVE_PEERS="$PEERS" \
    WARPDRIVE_RS_K="$K" \
    WARPDRIVE_RS_M="$M" \
    WARPDRIVE_ADMIN_ACCESS_KEY="$ACCESS_KEY" \
    WARPDRIVE_ADMIN_SECRET_KEY="$SECRET_KEY" \
    exec "$BIN" > node.out 2>&1
  ) &
  PIDS+=($!)
  echo "  node$i: pid $!, port $port, data: $node_dir"
done

echo
echo "Cluster is up. S3 endpoint on any node, e.g. http://127.0.0.1:${BASE_PORT}/cluster/s3/"
echo "Admin credentials: $ACCESS_KEY / $SECRET_KEY"
echo "Press Ctrl-C to stop."
wait
