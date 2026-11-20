#!/usr/bin/env bash
. /private/tmp/claude-502/-Users-cj-cj-personal-claude/d590bb8b-7213-4abc-b5c3-330a952fc604/scratchpad/gcp-cores/lib.sh
for k in $(seq 1 30); do rs wlc true 2>/dev/null && break; sleep 5; done
for i in 0 1 2; do ( for k in $(seq 1 30); do rs wln-$i true 2>/dev/null && break; sleep 5; done
  rs wln-$i 'DEV=$(lsblk -dpno NAME,MODEL | awk "/nvme_card|NVMe|EphemeralDisk/{print \$1; exit}"); [ -z "$DEV" ] && DEV=/dev/nvme0n1; mountpoint -q /mnt/nvme || { sudo mkfs.ext4 -F -q $DEV && sudo mkdir -p /mnt/nvme && sudo mount -o noatime $DEV /mnt/nvme && sudo chown $USER /mnt/nvme; }; mkdir -p ~/bins; echo "$(hostname): $(nproc) cpu, nvme $(df -h /mnt/nvme | tail -1 | awk "{print \$2}")"'
  rcp $O/server_log.yaml $O/sampler.sh wln-$i:~/bins/ ) & done
( rcp $O/src-par.tgz wlc:~/
  rs wlc 'set -e; command -v cc >/dev/null || { sudo DEBIAN_FRONTEND=noninteractive apt-get -qq update >/dev/null; sudo DEBIAN_FRONTEND=noninteractive apt-get -qq install -y build-essential pkg-config libsqlite3-dev protobuf-compiler python3-pip >/dev/null; pip3 install -q boto3 >/dev/null 2>&1; }
    [ -f $HOME/.cargo/env ] || curl -sSf https://sh.rustup.rs | sh -s -- -y -q --profile minimal >/dev/null 2>&1; . $HOME/.cargo/env
    rm -rf ~/src-par; mkdir -p ~/src-par ~/bins && tar xzf ~/src-par.tgz -C ~/src-par 2>/dev/null
    (cd ~/src-par/server && CARGO_TARGET_DIR=$HOME/tgt-par cargo build --release --bin warp_drive --bin s3_bench 2>&1 | tail -1)
    cp ~/tgt-par/release/warp_drive ~/bins/wd-par; cp ~/tgt-par/release/s3_bench ~/bins/
    [ -f ~/.ssh/id_ed25519 ] || ssh-keygen -q -t ed25519 -N "" -f ~/.ssh/id_ed25519; md5sum ~/bins/* | cut -c1-8,33-' ) &
wait
IPS=$(awk '/^wln-/{print $2}' $O/vms.txt | tr '\n' ' '); echo "$IPS" > $O/ips.txt
PUB=$(rs wlc 'cat ~/.ssh/id_ed25519.pub')
for i in 0 1 2; do rs wln-$i "grep -qF \"$PUB\" ~/.ssh/authorized_keys || echo \"$PUB\" >> ~/.ssh/authorized_keys" & done; wait
rcp $O/cores.sh $O/sampler.sh wlc:~/
rs wlc "cp ~/sampler.sh ~/bins/; for ip in $IPS; do scp -q -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR ~/bins/wd-par \$ip:~/bins/; done; echo $IPS > ~/ips.txt; echo shipped"
echo SETUP-DONE
