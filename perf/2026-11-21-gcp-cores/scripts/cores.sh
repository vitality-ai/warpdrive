#!/usr/bin/env bash
# Runs on wlc. Does node0 (4 vCPU) go faster with 4 cores than 2 at high client counts?
# Point = topology (single | rs21) x node0 cores (2 | 4, via taskset) x clients. #226 build only.
IPS=($(cat ~/ips.txt))
NSSH="ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -n"
DUR=${DUR:-120}; SIZE=4096; POINTS=${POINTS:-"single:2:128 single:4:128 single:2:512 single:4:512 rs21:2:128 rs21:4:128 rs21:2:512 rs21:4:512"}
AK=AKIAIOSFODNN7EXAMPLE; SK=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY
R=~/cores; mkdir -p $R; echo "start $(date -u +%T) DUR=$DUR" >> $R/progress.log
for p in $POINTS; do
  TOPO=${p%%:*}; rest=${p#*:}; CORES=${rest%%:*}; CONC=${rest#*:}; NAME=${TOPO}_${CORES}c_c$CONC
  D=$R/$NAME; rm -rf $D; mkdir -p $D
  if [ $TOPO = single ]; then N=1; PEERS=""; else N=3; PEERS=$(printf "http://%s:9710," "${IPS[@]}"); PEERS=${PEERS%,}; fi
  PIN=$(( CORES - 1 ))
  for i in 0 1 2; do $NSSH ${IPS[$i]} "pkill -x wd-par; pkill -f '[s]ampler.sh'; sleep 1; rm -rf /mnt/nvme/wd /mnt/nvme/mem.csv"; done
  for i in $(seq 0 $((N-1))); do
    TS=""; [ $i = 0 ] && TS="taskset -c 0-$PIN"
    SELF=""; [ -n "$PEERS" ] && SELF="WARPDRIVE_SELF_ADDR=http://${IPS[$i]}:9710 WARPDRIVE_PEERS=$PEERS WARPDRIVE_RS_K=2 WARPDRIVE_RS_M=1"
    $NSSH ${IPS[$i]} "mkdir -p /mnt/nvme/wd; cp ~/bins/server_log.yaml /mnt/nvme/wd/; cd /mnt/nvme/wd; env $SELF WARPDRIVE_PORT=9710 WARPDRIVE_ADMIN_ACCESS_KEY=$AK WARPDRIVE_ADMIN_SECRET_KEY=$SK setsid nohup $TS ~/bins/wd-par > node.out 2>&1 < /dev/null & sleep 1; setsid nohup bash ~/bins/sampler.sh wd-par /mnt/nvme/mem.csv > /dev/null 2>&1 < /dev/null &"
  done
  ok=0; for k in $(seq 1 30); do curl -s -o /dev/null -w '%{http_code}' http://${IPS[0]}:9710/ | grep -q '^[234]' && { ok=1; break; }; sleep 1; done
  [ $ok = 1 ] || { echo "$(date -u +%T) $NAME: node0 never answered" >> $R/progress.log; continue; }
  python3 -c "
import boto3; from botocore.config import Config
boto3.client('s3', endpoint_url='http://${IPS[0]}:9710', aws_access_key_id='$AK', aws_secret_access_key='$SK', region_name='us-east-1', config=Config(s3={'addressing_style':'path'})).create_bucket(Bucket='bench')" 2>> $D/bench.err
  setsid nohup bash ~/bins/sampler.sh - $D/client.csv > /dev/null 2>&1 < /dev/null & SP=$!
  ~/bins/s3_bench http://${IPS[0]}:9710 "" bench $CONC $DUR $SIZE > $D/bench.csv 2>> $D/bench.err
  kill $SP 2>/dev/null
  for i in $(seq 0 $((N-1))); do $NSSH ${IPS[$i]} "pkill -f '[s]ampler.sh'; taskset -cp \$(pgrep -x wd-par) | tail -c 20; pkill -x wd-par; grep -c -i 'error\|panicked' /mnt/nvme/wd/node.out" > $D/node$i.info 2>&1; scp -q -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR ${IPS[$i]}:/mnt/nvme/mem.csv $D/mem$i.csv; done
  echo "$(date -u +%T) $NAME done: $(tr '\n' ' ' < $D/bench.csv)" >> $R/progress.log
done
echo "ALL DONE $(date -u +%T)" >> $R/progress.log
