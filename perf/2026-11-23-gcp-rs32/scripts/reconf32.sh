#!/usr/bin/env bash
# Mac side: 5 x n2-standard-2 nodes (wln-0..4) and wlc resized to n2-standard-2, for RS(3,2). 12 vCPUs.
# wln-1/2 keep their disks and binaries. wln-0 (n2-standard-4) is replaced. wlc keeps its disk, toolchain and builds.
set -u
C=/private/tmp/claude-502/-Users-cj-cj-personal-claude/d590bb8b-7213-4abc-b5c3-330a952fc604/scratchpad/gcp-cores
Z="--zone us-central1-a"; COMMON="--image-family ubuntu-2204-lts --image-project ubuntu-os-cloud --boot-disk-size 30GB"
gcloud compute instances delete wln-0 $Z --quiet 2>&1 | grep -iE "error" 
gcloud compute instances stop wlc $Z --quiet 2>&1 | grep -iE "error"
gcloud compute instances set-machine-type wlc --machine-type n2-standard-2 $Z 2>&1 | grep -iE "error"
for n in wln-0 wln-3 wln-4; do gcloud compute instances create $n --machine-type n2-standard-2 $Z --local-ssd interface=NVME $COMMON 2>&1 | grep -iE "error|quota" & done; wait
gcloud compute instances start wlc $Z --quiet 2>&1 | grep -iE "error"
gcloud compute instances list --filter="name~^wl" --format="value(name,networkInterfaces[0].networkIP,networkInterfaces[0].accessConfigs[0].natIP,machineType.basename())" | sort > $C/vms-full.txt
cut -f1-3 $C/vms-full.txt > $C/vms.txt; cat $C/vms-full.txt; [ $(wc -l < $C/vms.txt) = 6 ] || { echo "VM set incomplete"; exit 1; }
. $C/lib.sh
for n in wlc wln-0 wln-1 wln-2 wln-3 wln-4; do for k in $(seq 1 40); do rs $n true 2>/dev/null && break; sleep 5; done; done
for n in wln-0 wln-3 wln-4; do rs $n 'DEV=$(lsblk -dpno NAME,MODEL | awk "/nvme_card|NVMe|EphemeralDisk/{print \$1; exit}"); [ -z "$DEV" ] && DEV=/dev/nvme0n1; mountpoint -q /mnt/nvme || { sudo mkfs.ext4 -F -q $DEV && sudo mkdir -p /mnt/nvme && sudo mount -o noatime $DEV /mnt/nvme && sudo chown $USER /mnt/nvme; }; mkdir -p ~/bins; echo "$(hostname): $(nproc) cpu, $(free -g | awk "/Mem/{print \$2}") GB, nvme $(df -h /mnt/nvme | tail -1 | awk "{print \$2}")"' & done; wait
for n in wln-1 wln-2; do rs $n 'mountpoint -q /mnt/nvme && echo "$(hostname): nvme mounted" || echo "$(hostname): NO NVME"'; done
PUB=$(rs wlc 'cat ~/.ssh/id_ed25519.pub')
for n in wln-0 wln-3 wln-4; do rs $n "grep -qF \"$PUB\" ~/.ssh/authorized_keys || echo \"$PUB\" >> ~/.ssh/authorized_keys"; done
IPS=$(awk '/^wln-/{print $2}' $C/vms.txt | tr '\n' ' '); echo "$IPS" > $C/ips.txt
rs wlc "echo $IPS > ~/ips.txt; nproc; for ip in $IPS; do scp -q -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR ~/bins/wd-final3 ~/bins/wd-rocks ~/bins/wd-combo ~/bins/comp_log.yaml ~/soak_sampler.sh \$ip:~/bins/ || echo \"copy to \$ip failed\"; done; echo shipped to $IPS"
echo RECONF-DONE
