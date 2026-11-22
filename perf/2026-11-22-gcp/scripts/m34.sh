#!/usr/bin/env bash
# Runs on wlc after m2_scanstall.sh: microbenchmarks 3 and 4 on wln-0's NVMe, with no server running.
until grep -q "ALL DONE\|ABORT" ~/micro/scanstall/progress.log 2>/dev/null; do sleep 30; done
R=~/micro/disk; rm -rf $R; mkdir -p $R
IPS=($(cat ~/ips.txt)); NSSH="ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -n"
$NSSH ${IPS[0]} "pkill -x wd-comp; pkill -x wd-cgc; sleep 1"
scp -q -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR ~/m34_disk.py ${IPS[0]}:~/
$NSSH ${IPS[0]} "findmnt -no SOURCE,FSTYPE /mnt/nvme; python3 ~/m34_disk.py /mnt/nvme/micro ~/m34.json; rm -rf /mnt/nvme/micro" > $R/out.txt 2>&1
scp -q -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR ${IPS[0]}:~/m34.json $R/
echo "$(date -u +%T) ALL DONE" >> $R/progress.log
