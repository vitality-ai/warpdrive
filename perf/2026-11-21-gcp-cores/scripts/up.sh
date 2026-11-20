#!/usr/bin/env bash
# wlc: persistent build + load client (n2-standard-2, never deleted by these scripts).
# wln-0: n2-standard-4 (the coordinator under test), wln-1/2: n2-standard-2. 10 of 12 vCPUs.
set -u
C=/private/tmp/claude-502/-Users-cj-cj-personal-claude/d590bb8b-7213-4abc-b5c3-330a952fc604/scratchpad/gcp-cores
Z="--zone us-central1-a"; COMMON="--image-family ubuntu-2204-lts --image-project ubuntu-os-cloud --boot-disk-size 30GB"
gcloud compute instances describe wlc $Z >/dev/null 2>&1 || gcloud compute instances create wlc --machine-type n2-standard-2 $Z $COMMON --labels keep=true 2>&1 | grep -iE "error|quota" &
gcloud compute instances create wln-0 --machine-type n2-standard-4 $Z --local-ssd interface=NVME $COMMON 2>&1 | grep -iE "error|quota" &
for i in 1 2; do gcloud compute instances create wln-$i --machine-type n2-standard-2 $Z --local-ssd interface=NVME $COMMON 2>&1 | grep -iE "error|quota" & done
wait
gcloud compute instances list --filter="name~^wl" --format="value(name,networkInterfaces[0].networkIP,networkInterfaces[0].accessConfigs[0].natIP)" | sort > $C/vms.txt
cat $C/vms.txt; [ $(wc -l < $C/vms.txt) = 4 ] || { echo "VM creation incomplete"; exit 1; }
