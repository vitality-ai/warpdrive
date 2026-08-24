#!/usr/bin/env bash
# Initialize neon_local against a given S3-compatible storage backend, create
# the tenant, and pre-create all 16 scaling-benchmark endpoints up front (so
# we never have to ad-hoc recreate a deleted endpoint mid-sweep).
#
# Usage:
#   ./init_cluster.sh <storage_host> <storage_port> <tenant_id> [pg_version]
#
# Example (MinIO baseline):
#   ./init_cluster.sh 10.128.0.4 9000 bf15ffc04f5f086e83febfff46d6774c 16
# Example (WarpDrive slab):
#   ./init_cluster.sh 10.128.0.4 9710 b2f51f6ebb6dbee89e761553264babe6 16

set -euo pipefail

STORAGE_HOST="${1:?storage host required}"
STORAGE_PORT="${2:?storage port required}"
TENANT_ID="${3:?tenant id required}"
PG_VERSION="${4:-16}"
PREPARE_THREADS="${5:-10}"  # scale=10 warehouses -> 10 threads = 1 warehouse/thread, no wasted threads

NEON_ROOT="/home/nash/cj/warpdrive/neon-src"
SYSBENCH_DIR="/home/nash/cj/warpdrive/sysbench-tpcc"
BUCKET="neon"
ADMIN_KEY="adminkey"
ADMIN_SECRET="adminsecretkey123456"

export AWS_ACCESS_KEY_ID="$ADMIN_KEY"
export AWS_SECRET_ACCESS_KEY="$ADMIN_SECRET"
export AWS_DEFAULT_REGION="us-east-1"
# On GCP, the AWS SDK's credential chain tries the EC2 Instance Metadata
# Service before env-var credentials; GCP's own metadata server answers that
# path with an unrelated 405 and the SDK retries slowly instead of failing
# fast. Must be set before `neon_local start` since pageserver (a long-running
# process) needs it in its own environment, not just the calling shell's.
export AWS_EC2_METADATA_DISABLED="true"
export PATH="$HOME/.local/bin:$NEON_ROOT/target/release:$NEON_ROOT/pg_install/v${PG_VERSION}/bin:$HOME/.cargo/bin:$PATH"

cd "$NEON_ROOT"

log() { echo "[$(date -u +%H:%M:%S)] $*"; }

# Endpoint roster: name, pg_port. internal_http = pg_port-2, external_http = pg_port-1
ENDPOINTS=(
  "main 55432"
  "ep-2 55451"
  "ep-5 55465"
  "ep-6 55466"
  "ep-a 55480"
  "ep-b 55481"
  "ep-e 55484"
  "ep-f 55485"
  "ep-g 55486"
  "ep-h 55487"
  "ep-k 55490"
  "ep-l 55492"
  "ep-m 55494"
  "ep-n 55496"
  "ep-o 55498"
  "ep-p 55500"
)

log "=== 1. Create bucket '$BUCKET' on $STORAGE_HOST:$STORAGE_PORT ==="
aws s3api create-bucket --bucket "$BUCKET" \
  --endpoint-url "http://${STORAGE_HOST}:${STORAGE_PORT}" 2>&1 | grep -v "BucketAlreadyOwnedByYou" || true

log "=== 2. neon_local init (wiping any prior .neon state) ==="
rm -rf "$NEON_ROOT/.neon"
neon_local init --force=empty-dir-ok

log "=== 3. Point pageserver at storage backend ==="
PS_TOML="$NEON_ROOT/.neon/pageserver_1/pageserver.toml"
python3 - "$PS_TOML" "$STORAGE_HOST" "$STORAGE_PORT" "$BUCKET" <<'PYEOF'
import re, sys
path, host, port, bucket = sys.argv[1:5]
with open(path) as f:
    content = f.read()
new_line = f"remote_storage = {{endpoint='http://{host}:{port}', bucket_name='{bucket}', bucket_region='us-east-1', prefix_in_bucket='/pageserver'}}"
if re.search(r'^remote_storage\s*=.*$', content, re.M):
    content = re.sub(r'^remote_storage\s*=.*$', new_line, content, flags=re.M)
else:
    content += "\n" + new_line + "\n"
with open(path, "w") as f:
    f.write(content)
print(f"  patched {path}")
PYEOF

log "=== 4. Point safekeeper at storage backend ==="
CFG="$NEON_ROOT/.neon/config"
python3 - "$CFG" "$STORAGE_HOST" "$STORAGE_PORT" "$BUCKET" <<'PYEOF'
import re, sys
path, host, port, bucket = sys.argv[1:5]
with open(path) as f:
    content = f.read()
remote = f"{{endpoint='http://{host}:{port}', bucket_name='{bucket}', bucket_region='us-east-1', prefix_in_bucket='/safekeeper/'}}"
if 'remote_storage' in content:
    content = re.sub(r'remote_storage\s*=\s*".*?"', f'remote_storage = "{remote}"', content, flags=re.S)
else:
    content = re.sub(r'(\[\[safekeepers\]\][^\[]*)', r'\1remote_storage = "' + remote + '"\n', content, count=1)
# avoid storcon wiping the safekeeper list on fresh init
if re.search(r'timelines_onto_safekeepers\s*=\s*true', content):
    content = re.sub(r'timelines_onto_safekeepers\s*=\s*true', 'timelines_onto_safekeepers = false', content)
elif 'timelines_onto_safekeepers' not in content:
    content += "\ntimelines_onto_safekeepers = false\n"
with open(path, "w") as f:
    f.write(content)
print(f"  patched {path}")
PYEOF

log "=== 5. Start pageserver + safekeeper ==="
neon_local start

log "=== 6. Create tenant $TENANT_ID ==="
neon_local tenant create --tenant-id "$TENANT_ID" --pg-version "$PG_VERSION" --set-default

log "=== 7. Create + start 'main' endpoint (55432) ==="
# HTTP ports use a range (60000s/61000s) fully disjoint from the pg_port
# range (55000s) and from each other -- deriving them as pg_port-1/pg_port-2
# collides whenever two endpoints in the roster have pg_ports <3 apart (most
# of this roster does). See KNOWN_ISSUES.md.
neon_local endpoint create main --tenant-id "$TENANT_ID" \
  --pg-port 55432 --internal-http-port 60000 --external-http-port 61000 \
  --pg-version "$PG_VERSION"
neon_local endpoint start main --start-timeout 120s

log "=== 8. sysbench prepare (scale=10, tables=1, threads=$PREPARE_THREADS) against main ==="
cd "$SYSBENCH_DIR"
sysbench tpcc.lua --pgsql-host=127.0.0.1 --pgsql-port=55432 \
  --pgsql-user=cloud_admin --pgsql-db=postgres \
  --threads="$PREPARE_THREADS" --tables=1 --scale=10 --db-driver=pgsql prepare
cd "$NEON_ROOT"

log "=== 9. Branch + create remaining 15 endpoints from main's post-prepare LSN ==="
idx=0
for entry in "${ENDPOINTS[@]:1}"; do
  idx=$((idx + 1))
  name=$(echo "$entry" | cut -d' ' -f1)
  port=$(echo "$entry" | cut -d' ' -f2)
  int_http=$((60000 + idx))
  ext_http=$((61000 + idx))
  log "  branching $name (pg_port=$port)"
  neon_local timeline branch --tenant-id "$TENANT_ID" \
    --branch-name "$name" --ancestor-branch-name main
  neon_local endpoint create "$name" --tenant-id "$TENANT_ID" \
    --branch-name "$name" \
    --pg-port "$port" --internal-http-port "$int_http" --external-http-port "$ext_http" \
    --pg-version "$PG_VERSION"
done

log "=== DONE: tenant $TENANT_ID initialized against ${STORAGE_HOST}:${STORAGE_PORT}, 16 endpoints created ==="
