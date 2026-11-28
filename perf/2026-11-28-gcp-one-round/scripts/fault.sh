#!/usr/bin/env bash
# fault.sh, on wlc: one-round PUTs under node crashes. Per arm and scenario: fresh cluster, loader.py PUTs through
# node 0 for 40 s, crash (SIGKILL) at 10 s, restart at 25 s with the same data. Then every acked object must read
# back intact from every node, and the count of failed PUTs that are visible anyway is recorded.
#   one      node 3 crashes
#   three    nodes 2, 3 and 4 crash (below the write quorum)
#   coord    node 0 crashes (the coordinator; the loader keeps sending to it)
R=${R:-~/or1-fault}; rm -rf $R; mkdir -p $R; exec > $R/driver.log 2>&1; log() { echo "$(date -u +%T) $*" | tee -a $R/progress.log; }
ARMS=${ARMS:-"two-all one one-quorum"}; SCEN=${SCEN:-"one three coord"}
IPS=($(cat ~/ips.txt)); NSSH="ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -n"
SCP="scp -q -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
AK=AKIAIOSFODNN7EXAMPLE; SK=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY; PEERS=$(printf "http://%s:9710," "${IPS[@]}"); PEERS=${PEERS%,}
BASE="WARPDRIVE_META_ENGINE=rocksdb WARPDRIVE_ROCKSDB_CACHE_MB=1024 WARPDRIVE_SEGMENT_MB=64 WARPDRIVE_RECLAIM_GRACE_SECS=120 WARPDRIVE_RECLAIM_CONFIRM_SECS=60 WARPDRIVE_COMPACT_SEAL_SECS=120"
KILL="for b in wd-244 wd-pr wd-prb wd-fe wd-z wd-redis ba wd-base wd-nat wd-v3 wd-1r; do pkill -x \$b; done"
for ip in "${IPS[@]}"; do $NSSH $ip "$KILL; sleep 3"; done
for ip in "${IPS[@]}"; do $SCP ~/bins/wd-base ~/bins/wd-1r $ip:bins/ || { log "ABORT scp $ip"; exit 1; }; done
ready() { for k in $(seq 1 300); do ok=0; for ip in "${IPS[@]}"; do curl -s -m 2 -o /dev/null -w '%{http_code}' http://$ip:9710/ | grep -q '^[234]' && ok=$((ok+1)); done; [ $ok = 5 ] && return 0; sleep 1; done; return 1; }
start() {  # start <ip> <binary> <extra env>: same data directory
  $NSSH $1 "cd /mnt/nvme/wd; ulimit -n 1048576; env $BASE $3 WARPDRIVE_SELF_ADDR=http://$1:9710 WARPDRIVE_PEERS=$PEERS WARPDRIVE_RS_K=3 WARPDRIVE_RS_M=2 WARPDRIVE_PORT=9710 WARPDRIVE_ADMIN_ACCESS_KEY=$AK WARPDRIVE_ADMIN_SECRET_KEY=$SK setsid nohup ~/bins/$2 >> node.out 2>&1 < /dev/null &"; }
fresh() {
  for ip in "${IPS[@]}"; do $NSSH $ip "$KILL; for k in \$(seq 1 60); do pgrep -x wd-base >/dev/null || pgrep -x wd-1r >/dev/null || break; sleep 1; done; for p in 9710 10710 11710; do for k in \$(seq 1 30); do ss -ltn | grep -q \":\$p \" || break; sleep 1; done; done; rm -rf /mnt/nvme/wd; mkdir -p /mnt/nvme/wd; cp ~/bins/comp_log.yaml /mnt/nvme/wd/server_log.yaml" & done; wait
  for ip in "${IPS[@]}"; do start $ip $1 "$2"; done
  ready || return 1; sleep 5; }
for scen in $SCEN; do for arm in $ARMS; do
  bin=wd-1r env=""; case $arm in two-all) bin=wd-base ;; one-quorum) env="WARPDRIVE_ROW_ACK=quorum" ;; esac
  case $scen in one) victims="3" ;; three) victims="2 3 4" ;; coord) victims="0" ;; esac
  tag=$scen-$arm; fresh $bin "$env" || { log "ABORT $tag: cluster not ready"; exit 1; }
  python3 ~/loader.py $R/$tag http://${IPS[0]}:9710 40 > $R/$tag-load.txt 2>&1 &
  sleep 10; for v in $victims; do $NSSH ${IPS[$v]} "pkill -9 -x $bin"; done; log "$tag: crashed $victims"
  sleep 15; for v in $victims; do for p in 9710 10710 11710; do $NSSH ${IPS[$v]} "for k in \$(seq 1 30); do ss -ltn | grep -q ':$p ' || break; sleep 1; done"; done; start ${IPS[$v]} $bin "$env"; done
  wait; ready || log "$tag: cluster not ready after restart"; sleep 10
  LOADER_CHECK=1 python3 ~/loader.py $R/$tag $(printf "http://%s:9710 " "${IPS[@]}") > $R/$tag-check.txt 2>&1; rc=$?
  log "$tag rc=$rc $(cat $R/$tag-load.txt | tail -1) | $(grep -o 'acked_wrong=[0-9]*.*' $R/$tag-check.txt | sed 's/acked=[0-9]* //' | tr '\n' ' ')"
  i=0; for ip in "${IPS[@]}"; do $NSSH $ip "grep -c -i panicked /mnt/nvme/wd/node.out" > $R/$tag-panics$i.txt; i=$((i+1)); done
done; done
log "FAULT DONE"
