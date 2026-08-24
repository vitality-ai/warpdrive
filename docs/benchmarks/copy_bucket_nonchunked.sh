#!/usr/bin/env bash
# Copy every object in a MinIO bucket to a WarpDrive bucket using plain
# single-shot PUT (aws s3api put-object), NOT `mc mirror` / `aws s3 sync` /
# `aws s3 cp` — those use aws-chunked streaming transfer encoding, and
# WarpDrive's PUT handler does not strip that framing before persisting the
# object, corrupting every object copied that way. put-object sends a plain
# body with a known Content-Length and round-trips cleanly.
#
# Usage: ./copy_bucket_nonchunked.sh <src_endpoint> <dst_endpoint> <bucket> [parallelism]

set -euo pipefail

SRC_EP="${1:?source endpoint required, e.g. http://127.0.0.1:9000}"
DST_EP="${2:?dest endpoint required, e.g. http://127.0.0.1:9710}"
BUCKET="${3:?bucket name required}"
PARALLEL="${4:-8}"

export AWS_ACCESS_KEY_ID=adminkey
export AWS_SECRET_ACCESS_KEY=adminsecretkey123456
export AWS_DEFAULT_REGION=us-east-1
export PATH="$HOME/.local/bin:$PATH"
MC="$HOME/minio/mc"

TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT

log() { echo "[$(date -u +%H:%M:%S)] $*"; }

log "Listing objects in $BUCKET..."
"$MC" ls --recursive "minio/$BUCKET" | awk '{print $NF}' > "$TMPDIR/keys.txt"
TOTAL=$(wc -l < "$TMPDIR/keys.txt")
log "Found $TOTAL objects. Copying with $PARALLEL parallel workers..."

copy_one() {
  local key="$1"
  local local_file
  local_file=$(mktemp)
  aws s3api get-object --bucket "$BUCKET" --key "$key" \
    --endpoint-url "$SRC_EP" "$local_file" > /dev/null
  # Replay the warpd-slab-hint metadata pageserver attaches on delta-layer
  # PUTs (see neon-src/pageserver/.../upload.rs) so WarpDrive's slab store
  # co-locates copied objects the same way it would for a live write. The
  # hint value is just the timeline_id, already embedded in the object's own
  # key path (pageserver/tenants/<tid>/timelines/<timeline_id>/...) -- no
  # need to read it back from the source.
  #
  # Co-location is delta-layer-only: image layers are already large,
  # standalone objects, so hinting them just bloats the slab file for no
  # benefit. Layer filenames distinguish the two:
  #   delta:  {key_range}__{LSN_START:016X}-{LSN_END:016X}[-{gen:08X}]
  #   image:  {key_range}__{LSN:016X}[-{gen:08X}]
  # i.e. a delta layer has two 16-hex-digit tokens after "__"; an image
  # layer has only one (its optional generation suffix is 8 hex digits, not
  # 16, so the two cases are unambiguous).
  local tid basename after is_delta
  tid=$(echo "$key" | sed -n 's#.*/timelines/\([0-9a-f]\{32\}\)/.*#\1#p')
  basename="${key##*/}"
  after="${basename#*__}"
  is_delta=false
  if [[ "$after" =~ ^[0-9A-Fa-f]{16}-[0-9A-Fa-f]{16} ]]; then
    is_delta=true
  fi
  if [ -n "$tid" ] && [ "$is_delta" = true ]; then
    aws s3api put-object --bucket "$BUCKET" --key "$key" \
      --body "$local_file" --metadata "warpd-slab-hint=$tid" \
      --endpoint-url "$DST_EP" > /dev/null
  else
    aws s3api put-object --bucket "$BUCKET" --key "$key" \
      --body "$local_file" --endpoint-url "$DST_EP" > /dev/null
  fi
  rm -f "$local_file"
}
export -f copy_one
export AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY AWS_DEFAULT_REGION BUCKET SRC_EP DST_EP

cat "$TMPDIR/keys.txt" | xargs -P "$PARALLEL" -I{} bash -c 'copy_one "$@"' _ {}

log "Copy complete: $TOTAL objects"
