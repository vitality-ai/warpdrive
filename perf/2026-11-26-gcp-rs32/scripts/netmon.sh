#!/usr/bin/env bash
# netmon.sh <out>: every 2 s: unix_s, NIC rx bytes, NIC tx bytes, NVMe sectors read, sectors written (cumulative).
while sleep 2; do
  echo "$(date +%s) $(awk '/ens|eth/{sub(/^ */,""); split($0,a,/[: ]+/); print a[2], a[10]; exit}' /proc/net/dev) $(awk '$3=="nvme0n1"||$3=="nvme0n1p2"{print $6, $10; exit}' /proc/diskstats)" >> "$1"
done
