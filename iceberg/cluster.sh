#!/usr/bin/env bash
set -u
BIN=/home/user/vitality-ai/warpdrive/server/target/release/warp_drive
LOGCFG=/home/user/vitality-ai/warpdrive/server/server_log.yaml
POLICY=/tmp/claude-0/-home-user/1092b597-fec1-566d-bf89-445018d387d4/scratchpad/policy.wasm
WORK=/tmp/wd-a1
N=5; BASE=9710
peers(){ local p=(); for ((i=0;i<N;i++)); do p+=("http://127.0.0.1:$((BASE+i))"); done; local IFS=,; echo "${p[*]}"; }
_our(){ for p in $(pgrep -x warp_drive 2>/dev/null); do tr '\0' '\n' </proc/$p/environ 2>/dev/null|grep -q "STORAGE_DIRECTORY=$WORK/" && echo $p; done; }
stop(){ for p in $(_our); do kill -9 $p 2>/dev/null; done; }
start(){
  stop; sleep 0.5; rm -rf "$WORK"; mkdir -p "$WORK"
  for ((i=0;i<N;i++)); do
    d="$WORK/node$i"; mkdir -p "$d/storage"; cp "$LOGCFG" "$d/" 2>/dev/null
    ( cd "$d" && exec env WARPDRIVE_PORT=$((BASE+i)) WARPDRIVE_PEERS="$(peers)" \
      WARPDRIVE_RS_K=3 WARPDRIVE_RS_M=2 \
      WARPDRIVE_LOCATION_LOG="$d/loc.log" WARPDRIVE_CONTENT_LOCATION_LOG="$d/cloc.log" \
      WARPDRIVE_BUCKET_CONFIG_LOG="$d/bcfg.log" WARPDRIVE_SHARD_META_LOG="$d/sm.log" \
      STORAGE_DIRECTORY="$d/storage" WARPDRIVE_WASM_PACKER="$POLICY" WARPDRIVE_ADMIN_ACCESS_KEY=adminkey WARPDRIVE_ADMIN_SECRET_KEY=adminsecret RUST_LOG=warn \
      "$BIN" >"$d/out.log" 2>&1 ) & 
  done
  for ((i=0;i<N;i++)); do for t in $(seq 1 100); do curl -fs -o /dev/null --max-time 1 "http://127.0.0.1:$((BASE+i))/cluster/_internal/timing" && break; sleep 0.2; done; done
  echo "cluster up; node0 wasm packer registered?"; grep -h 'registered WASM' "$WORK"/node0/out.log 2>/dev/null || echo "  (check logs)"
}
case "$1" in start) start;; stop) stop; echo stopped;; esac
