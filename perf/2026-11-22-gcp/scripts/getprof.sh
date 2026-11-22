#!/usr/bin/env bash
# Runs on wlc after hw.sh. Where does #226's GET lose to main? RS(2,1), node0 4 cores, 128 clients, 4 KB, 90 s PUT then 90 s GET.
# Arms: main (0326637) and ee6c06b (wd-ar + WARPDRIVE_ASYNC_READ=1). In the GET phase at +30 s on node0:
#   perf record -g dwarf 15 s, then perf stat + pidstat 10 s.
until grep -q "ALL DONE\|ABORT" ~/hw/progress.log 2>/dev/null; do sleep 20; done
DUR=90; IPS=($(cat ~/ips.txt)); P=~/gp; rm -rf $P; mkdir -p $P
NSSH="ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -n"
SCP="scp -q -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
AK=AKIAIOSFODNN7EXAMPLE; SK=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY
log() { echo "$(date -u +%T) $*" >> $P/progress.log; }
for ip in "${IPS[@]}"; do $NSSH $ip "mountpoint -q /mnt/nvme" || { log "ABORT no nvme $ip"; exit 1; }; done
log "bins: $(sha256sum ~/bins/wd-main ~/bins/wd-ar | cut -c1-16 | tr '\n' ' ')"
PEERS=$(printf "http://%s:9710," "${IPS[@]}"); PEERS=${PEERS%,}
for ARM in main ar; do
  BIN=wd-ar; ENVX="WARPDRIVE_ASYNC_READ=1"; [ $ARM = main ] && { BIN=wd-main; ENVX=""; }
  for i in 0 1 2; do $NSSH ${IPS[$i]} "pkill -x wd-main; pkill -x wd-ar; pkill -f '[s]ampler.sh'; sleep 1; rm -rf /mnt/nvme/wd /mnt/nvme/prof; mkdir -p /mnt/nvme/prof"; done
  for i in 0 1 2; do for p in 9710 10710 11710; do $NSSH ${IPS[$i]} "for k in \$(seq 1 30); do ss -ltn | grep -q ':$p ' || break; sleep 1; done"; done; done
  for i in 0 1 2; do $NSSH ${IPS[$i]} "mkdir -p /mnt/nvme/wd; cp ~/bins/server_log.yaml /mnt/nvme/wd/; cd /mnt/nvme/wd; env $ENVX WARPDRIVE_SELF_ADDR=http://${IPS[$i]}:9710 WARPDRIVE_PEERS=$PEERS WARPDRIVE_RS_K=2 WARPDRIVE_RS_M=1 WARPDRIVE_PORT=9710 WARPDRIVE_ADMIN_ACCESS_KEY=$AK WARPDRIVE_ADMIN_SECRET_KEY=$SK setsid nohup ~/bins/$BIN > node.out 2>&1 < /dev/null &"; done
  for k in $(seq 1 40); do curl -s -o /dev/null -w '%{http_code}' http://${IPS[0]}:9710/ | grep -q '^[234]' && break; sleep 1; done
  python3 -c "
import boto3; from botocore.config import Config
boto3.client('s3', endpoint_url='http://${IPS[0]}:9710', aws_access_key_id='$AK', aws_secret_access_key='$SK', region_name='us-east-1', config=Config(s3={'addressing_style':'path'})).create_bucket(Bucket='bench')"
  T0=$(date +%s)
  ~/bins/s3_bench http://${IPS[0]}:9710 "" bench 128 $DUR 4096 > $P/$ARM-bench.csv 2> $P/$ARM-bench.err &
  BP=$!
  sleep $(( T0 + DUR + 30 - $(date +%s) ))
  $NSSH ${IPS[0]} "cd /mnt/nvme/prof; setsid nohup sudo perf record -F 99 --call-graph dwarf,16384 -o get.data -p \$(pgrep -x $BIN) -- sleep 15 > /dev/null 2>&1 < /dev/null &"
  sleep 20
  $NSSH ${IPS[0]} "cd /mnt/nvme/prof; pid=\$(pgrep -x $BIN); sudo perf stat -e task-clock,context-switches,cpu-migrations -p \$pid -- sleep 10 2> perfstat.txt & pidstat -u -w -t -h -p \$pid 10 1 > pidstat.txt; wait"
  wait $BP
  $NSSH ${IPS[0]} "while pgrep -x perf > /dev/null; do sleep 2; done; pkill -x $BIN; cd /mnt/nvme/prof
    sudo perf report -i get.data --no-children --sort symbol --stdio -g none --percent-limit 0.2 2>/dev/null | grep -v '^#' | grep -v '^\$' > flat.txt
    sudo perf report -i get.data --no-children --sort dso --stdio -g none 2>/dev/null | grep -v '^#' | grep -v '^\$' > dso.txt
    sudo perf report -i get.data --children --sort symbol --stdio -g none --percent-limit 2 2>/dev/null | grep -v '^#' | grep -v '^\$' > inclusive.txt
    sudo perf script -i get.data 2>/dev/null | perl ~/bins/stackcollapse-perf.pl > get.folded; sudo chown \$USER *"
  mkdir -p $P/$ARM; $SCP "${IPS[0]}:/mnt/nvme/prof/*.txt" "${IPS[0]}:/mnt/nvme/prof/get.folded" $P/$ARM/
  log "$ARM done: $(tr '\n' ' ' < $P/$ARM-bench.csv)"
done
log "ALL DONE"
