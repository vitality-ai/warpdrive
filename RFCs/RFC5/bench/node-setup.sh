set -e
DEV=$(lsblk -dpno NAME,MODEL | awk '/nvme_card|NVMe|Google EphemeralDisk/{print $1; exit}')
[ -z "$DEV" ] && DEV=/dev/nvme0n1
sudo mkfs.ext4 -F -q "$DEV"
sudo mkdir -p /mnt/nvme && sudo mount -o noatime "$DEV" /mnt/nvme && sudo chown "$USER" /mnt/nvme
sudo apt-get -qq update >/dev/null && sudo DEBIAN_FRONTEND=noninteractive apt-get -qq install -y libsqlite3-0 curl >/dev/null
echo "$(hostname): data disk $DEV mounted at /mnt/nvme: $(df -h /mnt/nvme | tail -1 | awk '{print $2}')"
