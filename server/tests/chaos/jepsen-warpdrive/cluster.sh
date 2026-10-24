#!/usr/bin/env bash
# Cluster + fault control for the Jepsen/Elle WarpDrive harness.
# Single-host N-node RS(3,2) cluster. Faults the Clojure nemesis shells out to.
#
# Partition model (single-host, honest): we cannot give each node its own IP
# without iproute2 (no `ip`/veth here), so a "partition" isolates a node at
# the port level -- the process stays ALIVE but every peer's traffic to/from
# its port is dropped (iptables). That is strictly more than a process kill
# (the isolated node keeps its locks, timers and in-memory state while
# unreachable) and exercises quorum-under-partition, but it is per-node
# isolation, not a clean two-sided split-brain (which would need per-node
# IPs). Documented, not hidden.
set -u

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../../.." && pwd)"
SERVER_DIR="$REPO_ROOT/server"
BIN="$SERVER_DIR/target/release/warp_drive"
LOG_CFG="$SERVER_DIR/server_log.yaml"
WORK_DIR="${WD_WORK_DIR:-/tmp/wd-jepsen-clj}"
N="${WD_N:-5}"
BASE_PORT="${WD_BASE_PORT:-9710}"
ACCESS_KEY="${WD_ACCESS_KEY:-AKIAIOSFODNN7EXAMPLE}"
SECRET_KEY="${WD_SECRET_KEY:-wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY}"

# iptables needs root. In the dev container we run as root; on a CI runner we
# are not, so use sudo there. Detect once.
if [ "$(id -u)" = "0" ]; then SUDO=""; else SUDO="sudo"; fi

peers() { local p=(); for ((i=0;i<N;i++)); do p+=("http://127.0.0.1:$((BASE_PORT+i))"); done; local IFS=,; echo "${p[*]}"; }
node_dir() { echo "$WORK_DIR/node$1"; }
pidfile()  { echo "$WORK_DIR/node$1.pid"; }

start_node() {
  local i="$1" d; d="$(node_dir "$i")"
  mkdir -p "$d/storage"
  cp "$LOG_CFG" "$d/server_log.yaml" 2>/dev/null || true
  # Run from the node dir (the server loads server_log.yaml relative to cwd).
  # `exec env ... "$BIN"` so the backgrounded subshell BECOMES the node: the
  # recorded pid is warp_drive's real pid and no parent subshell lingers.
  # (The earlier `( ... nohup "$BIN" & echo $! )` form backgrounded the whole
  # compound, so `$!` was the subshell, the subshell stayed alive parenting
  # the node, and killing the recorded pid never killed the node -- that was
  # the leak + hang that silently reused stale processes.) FAILPOINTS is
  # inherited from the environment, so per-run failpoint config reaches nodes.
  ( cd "$d" && exec env \
    WARPDRIVE_PORT="$((BASE_PORT+i))" \
    WARPDRIVE_PEERS="$(peers)" \
    WARPDRIVE_RS_K=3 WARPDRIVE_RS_M=2 \
    WARPDRIVE_LOCATION_LOG="$d/location.log" \
    WARPDRIVE_CONTENT_LOCATION_LOG="$d/content_location.log" \
    WARPDRIVE_BUCKET_CONFIG_LOG="$d/bucket_config.log" \
    WARPDRIVE_SHARD_META_LOG="$d/shard_meta.log" \
    STORAGE_DIRECTORY="$d/storage" \
    WARPDRIVE_ADMIN_ACCESS_KEY="$ACCESS_KEY" \
    WARPDRIVE_ADMIN_SECRET_KEY="$SECRET_KEY" \
    RUST_LOG=warn \
    "$BIN" >"$d/stdout.log" 2>&1 ) &
  echo $! > "$(pidfile "$i")"
}

wait_healthy() {
  local i="$1" deadline=$(( $(date +%s) + ${2:-15} ))
  while (( $(date +%s) < deadline )); do
    if curl -fs -o /dev/null --max-time 1 "http://127.0.0.1:$((BASE_PORT+i))/cluster/_internal/timing"; then return 0; fi
    sleep 0.2
  done
  return 1
}

cmd_start() {
  [ -x "$BIN" ] || { echo "missing $BIN (build: cd server && cargo build --release --bin warp_drive)" >&2; exit 1; }
  # Pre-clean: kill any node still bound to our ports from a previous run,
  # so we never silently reuse a stale process (the bug that poisoned the
  # failpoint experiments). Verify the ports are actually free before we go.
  local i
  for ((i=0;i<N;i++)); do _kill_pids $(_pid_on_port "$((BASE_PORT+i))"); done
  sleep 0.5
  for ((i=0;i<N;i++)); do
    if [ -n "$(_pid_on_port "$((BASE_PORT+i))")" ]; then
      echo "port $((BASE_PORT+i)) still occupied after cleanup; aborting" >&2; exit 1
    fi
  done
  rm -rf "$WORK_DIR"; mkdir -p "$WORK_DIR"
  for ((i=0;i<N;i++)); do start_node "$i"; done
  for ((i=0;i<N;i++)); do wait_healthy "$i" 20 || { echo "node$i never healthy" >&2; exit 1; }; done
  echo "started $N nodes on $BASE_PORT..$((BASE_PORT+N-1))"
}

# Reliable pid lookup: read each live warp_drive's actual env rather than
# trusting a pidfile (the backgrounded-subshell pid was unreliable and left
# nodes alive across stop/start, so experiments silently ran against stale
# processes -- including stale FAILPOINTS). Scoped to THIS cluster via
# STORAGE_DIRECTORY under our WORK_DIR, so unrelated warp_drive is never hit.
_pid_on_port() {
  local port="$1" p
  for p in $(pgrep -x warp_drive 2>/dev/null); do
    if tr '\0' '\n' < "/proc/$p/environ" 2>/dev/null | grep -qx "WARPDRIVE_PORT=$port"; then echo "$p"; fi
  done
}
_our_pids() {
  local p
  for p in $(pgrep -x warp_drive 2>/dev/null); do
    if tr '\0' '\n' < "/proc/$p/environ" 2>/dev/null | grep -q "STORAGE_DIRECTORY=$WORK_DIR/"; then echo "$p"; fi
  done
}
_kill_pids() { local p; for p in "$@"; do kill -9 "$p" 2>/dev/null; done; }

cmd_kill() {
  local i="$1"
  _kill_pids $(_pid_on_port "$((BASE_PORT+i))")
  local pf; pf="$(pidfile "$i")"; [ -f "$pf" ] && kill -9 "$(cat "$pf")" 2>/dev/null; rm -f "$pf"
  echo "killed node$i"
}
cmd_restart() { local i="$1"; start_node "$i"; wait_healthy "$i" 20 && echo "restarted node$i" || echo "node$i restart UNHEALTHY"; }

# crash-at-fsync / power-loss: SIGKILL (no clean shutdown => no final flush of
# anything still only in the page cache), used by the durability nemesis.
cmd_crash()   { cmd_kill "$1"; }

cmd_isolate() {  # isolate node i from all peers (alive but unreachable)
  local i="$1" port=$((BASE_PORT+$1))
  $SUDO iptables -I INPUT  -p tcp --dport "$port" -j DROP 2>/dev/null
  $SUDO iptables -I OUTPUT -p tcp --sport "$port" -j DROP 2>/dev/null
  echo "isolated node$i (port $port) at network level; process still alive"
}
cmd_heal() {     # heal one node, or all
  if [ "${1:-all}" = "all" ]; then
    for ((i=0;i<N;i++)); do
      local port=$((BASE_PORT+i))
      while $SUDO iptables -D INPUT  -p tcp --dport "$port" -j DROP 2>/dev/null; do :; done
      while $SUDO iptables -D OUTPUT -p tcp --sport "$port" -j DROP 2>/dev/null; do :; done
    done
    echo "healed all partitions"
  else
    local port=$((BASE_PORT+$1))
    while $SUDO iptables -D INPUT  -p tcp --dport "$port" -j DROP 2>/dev/null; do :; done
    while $SUDO iptables -D OUTPUT -p tcp --sport "$port" -j DROP 2>/dev/null; do :; done
    echo "healed node$1"
  fi
}

cmd_corrupt() {  # flip one byte in a stored shard on node i
  local i="$1" d; d="$(node_dir "$i")/storage/admin"
  [ -d "$d" ] || { echo "no storage dir on node$i"; return 0; }
  local f; f="$(ls "$d" 2>/dev/null | head -1)"
  [ -n "$f" ] || { echo "no shard files on node$i"; return 0; }
  python3 - "$d/$f" <<'PY'
import sys,random
p=sys.argv[1]
with open(p,'r+b') as fh:
    data=fh.read()
    if len(data)<32: sys.exit(0)
    pos=random.randint(0,len(data)-1)
    fh.seek(pos); fh.write(bytes([data[pos]^0xFF]))
print(f"corrupted 1 byte at {pos} of {p}")
PY
}

# Power-loss: SIGKILL every node at once, NO clean shutdown and NO data wipe.
# Anything an acked write (HTTP 200) promised durable must survive this.
cmd_crashall() {
  _kill_pids $(_our_pids)
  for ((i=0;i<N;i++)); do rm -f "$(pidfile "$i")"; done
  echo "power-loss: SIGKILLed all $N nodes (data preserved on disk)"
}

# Bring the cluster back up on the EXISTING data dirs (no wipe).
cmd_recover() {
  for ((i=0;i<N;i++)); do start_node "$i"; done
  for ((i=0;i<N;i++)); do wait_healthy "$i" 25 || { echo "node$i never healthy after recover" >&2; exit 1; }; done
  echo "recovered all $N nodes on existing data"
}

cmd_stop() {
  # Kill every node of THIS cluster reliably (env-scoped), then verify.
  _kill_pids $(_our_pids)
  sleep 0.3
  local leftover; leftover="$(_our_pids)"
  if [ -n "$leftover" ]; then _kill_pids $leftover; sleep 0.3; fi
  for ((i=0;i<N;i++)); do rm -f "$(pidfile "$i")"; done
  cmd_heal all >/dev/null 2>&1
  echo "stopped cluster, healed partitions ($(_our_pids | wc -w) node procs remain)"
}

case "${1:-}" in
  start) cmd_start;;
  stop) cmd_stop;;
  kill) cmd_kill "$2";;
  restart) cmd_restart "$2";;
  crash) cmd_crash "$2";;
  isolate) cmd_isolate "$2";;
  heal) cmd_heal "${2:-all}";;
  corrupt) cmd_corrupt "$2";;
  crashall) cmd_crashall;;
  recover) cmd_recover;;
  peers) peers;;
  *) echo "usage: $0 {start|stop|kill i|restart i|crash i|isolate i|heal [i|all]|corrupt i|peers}" >&2; exit 2;;
esac
