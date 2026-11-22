#!/usr/bin/env bash
# Runs on wlc. HTTP worker A/B: main (0326637) vs #226 ee6c06b (wd-ar + WARPDRIVE_ASYNC_READ=1) at default workers (32) and at 4 (= cores).
# Topologies: single node (wln-0, 4 cores) and RS(2,1) (node0 4 cores). Sizes: 4 KB at 128 clients, 1 MB at 32 clients.

. $HOME/.cargo/env
R=~/hw; rm -rf $R; mkdir -p $R; log() { echo "$(date -u +%T) $*" >> $R/progress.log; }
IPS=($(cat ~/ips.txt)); NSSH="ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -n"
for ip in "${IPS[@]}"; do $NSSH $ip "mountpoint -q /mnt/nvme && findmnt -no SOURCE /mnt/nvme | grep -q nvme" || { log "ABORT: /mnt/nvme not on NVMe at $ip"; exit 1; }; done
log "bins: $(sha256sum ~/bins/wd-main ~/bins/wd-ar | cut -c1-16 | tr '\n' ' ')"
AK=AKIAIOSFODNN7EXAMPLE; SK=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY; PEERS=$(printf "http://%s:9710," "${IPS[@]}"); PEERS=${PEERS%,}
DUR=${DUR:-60}; run=0
for TOPO in single rs21; do for SZ in 4096:128; do for ARM in main w32 w4 w4 w32 main; do
  run=$((run+1)); SIZE=${SZ%%:*}; CONC=${SZ#*:}; NAME=r${run}_${TOPO}_${SIZE}_${ARM}; D=$R/$NAME; mkdir -p $D
  BIN=wd-ar; ENVX="WARPDRIVE_ASYNC_READ=1"; [ $ARM = main ] && { BIN=wd-main; ENVX=""; }; [ $ARM = w4 ] && ENVX="$ENVX WARPDRIVE_HTTP_WORKERS=4"
  N=3; [ $TOPO = single ] && N=1
  for i in 0 1 2; do $NSSH ${IPS[$i]} "pkill -x wd-main; pkill -x wd-226; pkill -x wd-par; pkill -x wd-ar; pkill -f '[s]ampler.sh'; sleep 1; rm -rf /mnt/nvme/wd /mnt/nvme/mem.csv"; done
  for i in 0 1 2; do for p in 9710 10710 11710; do $NSSH ${IPS[$i]} "for k in \$(seq 1 30); do ss -ltn | grep -q ':$p ' || break; sleep 1; done"; done; done
  for i in $(seq 0 $((N-1))); do CL=""; [ $TOPO = rs21 ] && CL="WARPDRIVE_SELF_ADDR=http://${IPS[$i]}:9710 WARPDRIVE_PEERS=$PEERS WARPDRIVE_RS_K=2 WARPDRIVE_RS_M=1"
    $NSSH ${IPS[$i]} "mkdir -p /mnt/nvme/wd; cp ~/bins/server_log.yaml /mnt/nvme/wd/; cd /mnt/nvme/wd; env $ENVX $CL WARPDRIVE_PORT=9710 WARPDRIVE_ADMIN_ACCESS_KEY=$AK WARPDRIVE_ADMIN_SECRET_KEY=$SK setsid nohup ~/bins/$BIN > node.out 2>&1 < /dev/null & sleep 1; setsid nohup bash ~/bins/sampler.sh $BIN /mnt/nvme/mem.csv > /dev/null 2>&1 < /dev/null &"; done
  ok=0; for k in $(seq 1 40); do curl -s -o /dev/null -w '%{http_code}' http://${IPS[0]}:9710/ | grep -q '^[234]' && { ok=1; break; }; sleep 1; done
  [ $ok = 1 ] || { log "$NAME: node0 never answered"; continue; }
  python3 -c "
import boto3; from botocore.config import Config
boto3.client('s3', endpoint_url='http://${IPS[0]}:9710', aws_access_key_id='$AK', aws_secret_access_key='$SK', region_name='us-east-1', config=Config(s3={'addressing_style':'path'})).create_bucket(Bucket='bench')" 2>> $D/bench.err
  setsid nohup bash ~/bins/sampler.sh - $D/client.csv > /dev/null 2>&1 < /dev/null & SP=$!
  ~/bins/s3_bench http://${IPS[0]}:9710 "" bench $CONC $DUR $SIZE > $D/bench.csv 2>> $D/bench.err
  kill $SP 2>/dev/null
  for i in $(seq 0 $((N-1))); do $NSSH ${IPS[$i]} "pkill -f '[s]ampler.sh'; pkill -x $BIN; grep -c -i 'panicked' /mnt/nvme/wd/node.out" > $D/node$i.panics 2>&1; scp -q -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR ${IPS[$i]}:/mnt/nvme/mem.csv $D/mem$i.csv; done
  log "$NAME done: $(tr '\n' ' ' < $D/bench.csv)"
done; done; done
log "ALL DONE"
