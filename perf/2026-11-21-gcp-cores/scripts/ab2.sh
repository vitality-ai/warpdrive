#!/usr/bin/env bash
# Runs on wlc. Async-metadata A/B: one binary (b62e763 + WARPDRIVE_ASYNC_META switch), RS(2,1), node0 pinned to 2 or 4 cores, 128 clients.
# Arms alternate so drift over time hits both equally. Records node0 context switches at phase boundaries.
IPS=($(cat ~/ips.txt))
NSSH="ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -n"
DUR=${DUR:-120}; SIZE=4096; CONC=128
POINTS=${POINTS:-"sync:2 async:2 sync:4 async:4 async:2 sync:2 async:4 sync:4"}
AK=AKIAIOSFODNN7EXAMPLE; SK=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY
PEERS=$(printf "http://%s:9710," "${IPS[@]}"); PEERS=${PEERS%,}
R=~/ab2; mkdir -p $R; echo "start $(date -u +%T) DUR=$DUR" >> $R/progress.log
threads() { $NSSH ${IPS[0]} 'p=$(pgrep -x wd-par); for t in /proc/$p/task/*; do echo "$(cat $t/comm)|$(awk "/^voluntary_ctxt/{v=\$2} /^nonvoluntary_ctxt/{n=\$2} END{print v\"|\"n}" $t/status)|$(awk "{print \$14+\$15}" $t/stat)|$(basename $t)"; done'; }
cs() { $NSSH ${IPS[0]} 'p=$(pgrep -x wd-par); cat /proc/$p/task/*/status 2>/dev/null | awk "/ctxt_switches/{s+=\$2} END{print s}"; awk "{print \$14+\$15}" /proc/$p/stat'; }
run=0
for p in $POINTS; do
  run=$((run+1)); ARM=${p%%:*}; CORES=${p#*:}; NAME=r${run}_${ARM}_${CORES}c; D=$R/$NAME; rm -rf $D; mkdir -p $D
  ENVX=""; [ $ARM = async ] && ENVX="WARPDRIVE_ASYNC_META=1"
  for i in 0 1 2; do $NSSH ${IPS[$i]} "pkill -x wd-par; pkill -f '[s]ampler.sh'; sleep 1; rm -rf /mnt/nvme/wd /mnt/nvme/mem.csv"; done
  for i in 0 1 2; do TS=""; [ $i = 0 ] && TS="taskset -c 0-$((CORES-1))"
    $NSSH ${IPS[$i]} "mkdir -p /mnt/nvme/wd; cp ~/bins/server_log.yaml /mnt/nvme/wd/; cd /mnt/nvme/wd; env $ENVX WARPDRIVE_SELF_ADDR=http://${IPS[$i]}:9710 WARPDRIVE_PEERS=$PEERS WARPDRIVE_RS_K=2 WARPDRIVE_RS_M=1 WARPDRIVE_PORT=9710 WARPDRIVE_ADMIN_ACCESS_KEY=$AK WARPDRIVE_ADMIN_SECRET_KEY=$SK setsid nohup $TS ~/bins/wd-par > node.out 2>&1 < /dev/null & sleep 1; setsid nohup bash ~/bins/sampler.sh wd-par /mnt/nvme/mem.csv > /dev/null 2>&1 < /dev/null &"; done
  ok=0; for k in $(seq 1 30); do curl -s -o /dev/null -w '%{http_code}' http://${IPS[0]}:9710/ | grep -q '^[234]' && { ok=1; break; }; sleep 1; done
  [ $ok = 1 ] || { echo "$(date -u +%T) $NAME: node0 never answered" >> $R/progress.log; continue; }
  python3 -c "
import boto3; from botocore.config import Config
boto3.client('s3', endpoint_url='http://${IPS[0]}:9710', aws_access_key_id='$AK', aws_secret_access_key='$SK', region_name='us-east-1', config=Config(s3={'addressing_style':'path'})).create_bucket(Bucket='bench')" 2>> $D/bench.err
  setsid nohup bash ~/bins/sampler.sh - $D/client.csv > /dev/null 2>&1 < /dev/null & SP=$!
  T0=$(date +%s); echo "start $T0 $(cs | tr '\n' ' ')" > $D/cs.txt; threads > $D/threads-start.txt
  ( sleep $DUR; echo "put_end $(date +%s) $(cs | tr '\n' ' ')" >> $D/cs.txt; threads > $D/threads-put_end.txt ) &
  CSP=$!
  ~/bins/s3_bench http://${IPS[0]}:9710 "" bench $CONC $DUR $SIZE > $D/bench.csv 2>> $D/bench.err
  echo "get_end $(date +%s) $(cs | tr '\n' ' ')" >> $D/cs.txt; threads > $D/threads-get_end.txt; wait $CSP
  kill $SP 2>/dev/null
  for i in 0 1 2; do $NSSH ${IPS[$i]} "pkill -f '[s]ampler.sh'; pkill -x wd-par; grep -c -i 'panicked' /mnt/nvme/wd/node.out" > $D/node$i.panics 2>&1; scp -q -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR ${IPS[$i]}:/mnt/nvme/mem.csv $D/mem$i.csv; done
  echo "$(date -u +%T) $NAME done: $(tr '\n' ' ' < $D/bench.csv) | $(tr '\n' ' ' < $D/cs.txt)" >> $R/progress.log
done
echo "ALL DONE $(date -u +%T)" >> $R/progress.log
