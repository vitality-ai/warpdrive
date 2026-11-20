#!/usr/bin/env bash
# sampler.sh <binary name|-> <out csv>: every 5 s: unix_s, server RSS KB, data dir KB, CPU busy %, NVMe busy %.
read_cpu() { awk '/^cpu /{idle=$5+$6; t=0; for(i=2;i<=NF;i++) t+=$i; print t, idle}' /proc/stat; }
read_disk() { awk '$3 ~ /^nvme0n1$/{print $13}' /proc/diskstats; }
set -- "$@"; prev=($(read_cpu)); pd=$(read_disk); pt=$(date +%s%N)
while true; do
  sleep 5
  cur=($(read_cpu)); cd=$(read_disk); ct=$(date +%s%N)
  dt=$((cur[0]-prev[0])); di=$((cur[1]-prev[1])); busy=$(( dt>0 ? 100*(dt-di)/dt : 0 ))
  ms=$(( (ct-pt)/1000000 )); du=$(( ms>0 && -n "$cd" ? 100*(${cd:-0}-${pd:-0})/ms : 0 ))
  rss=$([ "$1" = - ] && echo 0 || ps -o rss= -C $1 | head -1 | tr -d ' ')
  echo "$(date +%s),${rss:-0},$(du -sk /mnt/nvme/wd 2>/dev/null | cut -f1),$busy,$du" >> $2
  prev=(${cur[@]}); pd=$cd; pt=$ct
done
