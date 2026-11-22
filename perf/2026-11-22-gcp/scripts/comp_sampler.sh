#!/usr/bin/env bash
# comp_sampler.sh <binary> <out csv>: every 5 s: unix_s, RSS KB, segment KB, segment files, metadata logs KB, CPU busy %, NVMe busy %.
read_cpu() { awk '/^cpu /{idle=$5+$6; t=0; for(i=2;i<=NF;i++) t+=$i; print t, idle}' /proc/stat; }
read_disk() { awk '$3=="nvme0n1"||$3=="nvme0n1p2"{print $13; exit}' /proc/diskstats; }
prev=($(read_cpu)); pd=$(read_disk); pt=$(date +%s%N)
while true; do
  sleep 5
  cur=($(read_cpu)); cd=$(read_disk); ct=$(date +%s%N)
  dt=$((cur[0]-prev[0])); di=$((cur[1]-prev[1])); busy=0; [ "$dt" -gt 0 ] && busy=$((100*(dt-di)/dt))
  ms=$(( (ct-pt)/1000000 )); du=0; [ "$ms" -gt 0 ] && du=$((100*(cd-pd)/ms))
  rss=$(ps -o rss= -C "$1" | head -1 | tr -d ' ')
  seg=$(find /mnt/nvme/wd/storage -name '*.seg' -printf '%k %p\n' 2>/dev/null | awk '{k+=$1; n++} END{print k+0","n+0}')
  meta=$(du -ck /mnt/nvme/wd/*.log 2>/dev/null | tail -1 | cut -f1)
  echo "$(date +%s),${rss:-0},$seg,${meta:-0},$busy,$du" >> "$2"
  prev=(${cur[@]}); pd=$cd; pt=$ct
done
