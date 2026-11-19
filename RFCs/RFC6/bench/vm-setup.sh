#!/usr/bin/env bash
# Runs on each VM. Splits the local NVMe SSD into an XFS and an ext4
# partition, installs Rust, and builds segbench from ~/segbench.
set -e
DEV=$(lsblk -dpno NAME,MODEL | awk '/nvme_card|NVMe|Google EphemeralDisk/{print $1; exit}')
[ -z "$DEV" ] && DEV=/dev/nvme0n1
sudo apt-get -qq update >/dev/null
sudo DEBIAN_FRONTEND=noninteractive apt-get -qq install -y build-essential xfsprogs parted >/dev/null
sudo parted -s "$DEV" mklabel gpt mkpart xfs 0% 50% mkpart ext4 50% 100%
sleep 2
sudo mkfs.xfs -f -q "${DEV}p1"
sudo mkfs.ext4 -F -q "${DEV}p2"
sudo mkdir -p /mnt/xfs /mnt/ext4
sudo mount -o noatime "${DEV}p1" /mnt/xfs && sudo mount -o noatime "${DEV}p2" /mnt/ext4
sudo chown "$USER" /mnt/xfs /mnt/ext4
curl -sSf https://sh.rustup.rs | sh -s -- -y -q --profile minimal >/dev/null 2>&1
. "$HOME/.cargo/env"
cd ~/segbench && cargo build --release -q 2>&1 | tail -2
echo "$(hostname): xfs reflink=$(sudo xfs_info /mnt/xfs | grep -o 'reflink=[01]'), $(df -h /mnt/xfs /mnt/ext4 | tail -2 | awk '{print $6"="$2}' | tr '\n' ' ')"
