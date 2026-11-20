#!/usr/bin/env bash
# Runs on the client after the long run: saturation sweep over client concurrency.
until grep -q "ALL DONE" ~/res/progress.log 2>/dev/null; do sleep 30; done
sleep 120   # leave time for the Mac to pull the long run's results
mkdir -p ~/sweep; cp ~/res/progress.log ~/sweep/longrun-progress.log
for c in 32 64 128 256 512; do
  for v in "217:wd-217:" "par:wd-par:" "par_w256:wd-par:WARPDRIVE_HTTP_WORKERS=256"; do
    name=${v%%:*}_c$c
    setsid nohup bash ~/bins/sampler.sh - ~/sweep/client-$name.csv > /dev/null 2>&1 < /dev/null &
    SPID=$!
    CONFIGS="$name:${v#*:}" CONC=$c DUR=120 bash ~/longrun.sh >> ~/sweep/sweep.out 2>&1
    kill $SPID 2>/dev/null
    mv ~/res/$name ~/sweep/ 2>/dev/null
    tail -1 ~/res/progress.log | sed "s/^/[sweep] /" >> ~/sweep/progress.log
  done
done
echo "SWEEP DONE $(date -u +%T)" >> ~/sweep/progress.log
