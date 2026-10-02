#!/usr/bin/env bash
# Per-node storage backing: loopback device + XFS, per
# docs/Distributed-Engine-Plan.md. Linux-only (losetup, mkfs.xfs) — run on
# the real GCP VMs, not on a macOS dev machine. Needs root (loop devices,
# mkfs, mount all require CAP_SYS_ADMIN).
#
# Usage: sudo ./setup_xfs_storage.sh <backing-file-path> <size-in-gb> <mount-point>
# Example: sudo ./setup_xfs_storage.sh /var/lib/warpdrive/node0.img 20 /mnt/warpdrive-node0

set -euo pipefail

if [ "$(id -u)" -ne 0 ]; then
    echo "error: must run as root (losetup/mkfs.xfs/mount need CAP_SYS_ADMIN)" >&2
    exit 1
fi

if ! command -v losetup >/dev/null || ! command -v mkfs.xfs >/dev/null; then
    echo "error: losetup and mkfs.xfs are required (Linux only, package xfsprogs)" >&2
    exit 1
fi

BACKING_FILE="${1:?backing file path required}"
SIZE_GB="${2:?size in GB required}"
MOUNT_POINT="${3:?mount point required}"

mkdir -p "$(dirname "$BACKING_FILE")" "$MOUNT_POINT"

if [ ! -f "$BACKING_FILE" ]; then
    echo "Allocating ${SIZE_GB}GB backing file at $BACKING_FILE"
    fallocate -l "${SIZE_GB}G" "$BACKING_FILE"
fi

LOOP_DEV=$(losetup --find --show "$BACKING_FILE")
echo "Backing file attached at $LOOP_DEV"

# Only format if it doesn't already look like an XFS filesystem (idempotent re-run).
if ! blkid -o value -s TYPE "$LOOP_DEV" 2>/dev/null | grep -q xfs; then
    echo "Formatting $LOOP_DEV as XFS"
    mkfs.xfs "$LOOP_DEV"
fi

mount "$LOOP_DEV" "$MOUNT_POINT"
echo "Mounted $LOOP_DEV at $MOUNT_POINT"
echo
echo "Set STORAGE_DIRECTORY=$MOUNT_POINT in the environment before starting warp_drive."
