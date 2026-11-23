#!/usr/bin/env bash
# Extra guard for the sizing run: nodes 1-2 have 8 GB, so stop the load past 6 GB RSS on any node.
IPS=($(cat ~/ips.txt)); NSSH="ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -n"
while ! grep -q "ALL DONE\|ABORT" ~/sizing/progress.log 2>/dev/null; do
  for i in 0 1 2; do
    r=$($NSSH ${IPS[$i]} "ps -o rss= -C wd-final3 | head -1" | tr -d ' ')
    if [ -n "$r" ] && [ "$r" -gt 6291456 ] && pgrep -x s3b-comp >/dev/null; then
      echo "$(date -u +%T) GUARD node $i rss ${r} KB (6 GB guard: nodes 1-2 have 8 GB)" >> ~/sizing/progress.log; pkill -x s3b-comp
    fi
  done
  sleep 5
done
