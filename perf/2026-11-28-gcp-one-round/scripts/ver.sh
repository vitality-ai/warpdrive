#!/usr/bin/env bash
# ver.sh, on wlc: content check of one-round PUTs. Per arm: fresh cluster, verify.py PUTs through node 0 and reads
# every object back from all 5 nodes. Then node 4 is stopped and node 0 reads every object again (degraded, RS(3,2)
# still has 4 of 5 shards).
R=${R:-~/or1-verify}; rm -rf $R; mkdir -p $R; exec > $R/driver.log 2>&1; log() { echo "$(date -u +%T) $*" | tee -a $R/progress.log; }
ARMS=${ARMS:-"two-all two-quorum one one-quorum"}
IPS=($(cat ~/ips.txt)); NSSH="ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -n"
SCP="scp -q -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
AK=AKIAIOSFODNN7EXAMPLE; SK=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY; PEERS=$(printf "http://%s:9710," "${IPS[@]}"); PEERS=${PEERS%,}
BASE="WARPDRIVE_META_ENGINE=rocksdb WARPDRIVE_ROCKSDB_CACHE_MB=1024 WARPDRIVE_SEGMENT_MB=64 WARPDRIVE_RECLAIM_GRACE_SECS=120 WARPDRIVE_RECLAIM_CONFIRM_SECS=60 WARPDRIVE_COMPACT_SEAL_SECS=120"
KILL="for b in wd-244 wd-pr wd-prb wd-fe wd-z wd-redis ba wd-base wd-nat wd-v3 wd-1r; do pkill -x \$b; done"
for ip in "${IPS[@]}"; do $NSSH $ip "$KILL; sleep 3"; done
for ip in "${IPS[@]}"; do $SCP ~/bins/wd-base ~/bins/wd-1r $ip:bins/ || { log "ABORT scp $ip"; exit 1; }; done
ready() { for k in $(seq 1 300); do ok=0; for ip in "${IPS[@]}"; do curl -s -m 2 -o /dev/null -w '%{http_code}' http://$ip:9710/ | grep -q '^[234]' && ok=$((ok+1)); done; [ $ok = 5 ] && return 0; sleep 1; done; return 1; }
fresh() {  # fresh <binary> <extra env>
  for ip in "${IPS[@]}"; do $NSSH $ip "$KILL; for k in \$(seq 1 60); do pgrep -x wd-base >/dev/null || pgrep -x wd-1r >/dev/null || break; sleep 1; done; for p in 9710 10710 11710; do for k in \$(seq 1 30); do ss -ltn | grep -q \":\$p \" || break; sleep 1; done; done; rm -rf /mnt/nvme/wd; mkdir -p /mnt/nvme/wd; cp ~/bins/comp_log.yaml /mnt/nvme/wd/server_log.yaml" & done; wait
  for ip in "${IPS[@]}"; do $NSSH $ip "cd /mnt/nvme/wd; ulimit -n 1048576; env $BASE $2 WARPDRIVE_SELF_ADDR=http://$ip:9710 WARPDRIVE_PEERS=$PEERS WARPDRIVE_RS_K=3 WARPDRIVE_RS_M=2 WARPDRIVE_PORT=9710 WARPDRIVE_ADMIN_ACCESS_KEY=$AK WARPDRIVE_ADMIN_SECRET_KEY=$SK setsid nohup ~/bins/$1 > node.out 2>&1 < /dev/null &"; done
  ready || return 1; sleep 5; }
for arm in $ARMS; do
  bin=wd-1r env=""; case $arm in two-all) bin=wd-base ;; two-quorum) env="WARPDRIVE_BLOB_WITH_ROW=off" ;; one-quorum) env="WARPDRIVE_ROW_ACK=quorum" ;; esac
  fresh $bin "$env" || { log "ABORT $arm: cluster not ready"; exit 1; }
  python3 ~/verify.py $R/$arm.json $(printf "http://%s:9710 " "${IPS[@]}") > $R/$arm-all.txt 2>&1; rc=$?
  log "$arm all nodes rc=$rc: $(grep -o 'objects=.*errors=[0-9]*' $R/$arm-all.txt | tr '\n' ' ')"
  $NSSH ${IPS[4]} "pkill -x $bin"; sleep 3
  VERIFY_READ_ONLY=1 python3 ~/verify.py $R/$arm.json http://${IPS[0]}:9710 > $R/$arm-degraded.txt 2>&1; rc=$?
  log "$arm node 4 down rc=$rc: $(grep -o 'objects=.*errors=[0-9]*' $R/$arm-degraded.txt)"
  i=0; for ip in "${IPS[@]}"; do $NSSH $ip "grep -c -i panicked /mnt/nvme/wd/node.out" > $R/$arm-panics$i.txt; i=$((i+1)); done
done
log "VERIFY DONE"
