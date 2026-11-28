#!/usr/bin/env bash
# or1.sh, on wlc, detached: one-round PUT (feat-one-round-put) against #250, 5 x n2-standard-2, RS(3,2), RocksDB.
# Builds: wd-base (#250 head, from zs3.sh) and wd-1r (~/src-1r.tgz, default flags).
# Arms:
#   two-all     wd-base                               shards then row, each round waits for every peer
#   two-quorum  wd-1r WARPDRIVE_BLOB_WITH_ROW=off     shards then row, the shard round returns at a quorum
#   one         wd-1r                                 shards with the row, waits for every peer
#   one-quorum  wd-1r WARPDRIVE_ROW_ACK=quorum        shards with the row, returns at a quorum including node 0
# Each arm at SPECS, with the Azure geo round trips added by netem (geo) and without (zone), REPS runs, arm order
# rotated every run. Per run: fresh cluster, 60 s PUT then 60 s GET from s3_bench on wlc to node 0, CPU and network on
# every node, and node 0's /cluster/_internal/timing (round and phase breakdown) at the end of the PUT phase.
source ~/.cargo/env; R=${R:-~/or1}; rm -rf $R; mkdir -p $R/raw; exec > $R/driver.log 2>&1; log() { echo "$(date -u +%T) $*" | tee -a $R/progress.log; }
REPS=${REPS:-2}; DUR=${DUR:-60}; SPECS=${SPECS:-"4096:64 65536:64 1048576:32"}; NETS=${NETS:-"geo zone"}; ARMS=${ARMS:-"two-all two-quorum one one-quorum"}
log "build"
rm -rf ~/src-1r; mkdir -p ~/src-1r; tar xzf ~/src-1r.tgz -C ~/src-1r
(cd ~/src-1r/server && CARGO_TARGET_DIR=~/tgt-1r cargo build --release -q --bin warp_drive) && cp ~/tgt-1r/release/warp_drive ~/bins/wd-1r || { log "BUILD FAILED"; exit 1; }
log "wd-base $(sha256sum ~/bins/wd-base | cut -c1-12) wd-1r $(sha256sum ~/bins/wd-1r | cut -c1-12) s3b-z3 $(sha256sum ~/bins/s3b-z3 | cut -c1-12) src $(cat ~/src-1r/COMMIT 2>/dev/null)"
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
echo "net,arm,size,clients,rep,phase,ops,errors,ops_per_s,mean_us,p50_us,p90_us,p99_us" > $R/summary.csv
run() {  # run <net> <arm> <size> <clients> <rep>
  local net=$1 arm=$2 size=$3 clients=$4 rep=$5 bin=wd-1r env=""
  case $arm in two-all) bin=wd-base ;; two-quorum) env="WARPDRIVE_BLOB_WITH_ROW=off" ;; one-quorum) env="WARPDRIVE_ROW_ACK=quorum" ;; esac
  local tag=$net-$arm-$size-c$clients-r$rep
  fresh $bin "$env" || { log "ABORT $tag: cluster not ready"; exit 1; }
  for ip in "${IPS[@]}"; do $NSSH $ip "rm -f /mnt/nvme/net.txt /mnt/nvme/s.csv; setsid nohup bash ~/bins/netmon.sh /mnt/nvme/net.txt > /dev/null 2>&1 < /dev/null & echo \$! > /mnt/nvme/netmon.pid; setsid nohup bash ~/bins/soak_sampler.sh $bin /mnt/nvme/s.csv > /dev/null 2>&1 < /dev/null & echo \$! > /mnt/nvme/sampler.pid"; done
  sleep 3; local t0=$(date +%s)
  # Node 0's timing after the PUT phase: the GET phase writes no rows, so the counters then hold only PUTs.
  ( sleep $((DUR + 2)); curl -s -m 10 http://${IPS[0]}:9710/cluster/_internal/timing > $R/raw/$tag-timing.txt ) &
  S3B_DATA=text ~/bins/s3b-z3 http://${IPS[0]}:9710 "" bench $clients $DUR $size > $R/raw/$tag.csv 2> $R/raw/$tag.err
  wait; local t1=$(date +%s); sleep 3
  local i=0; for ip in "${IPS[@]}"; do $NSSH $ip "kill \$(cat /mnt/nvme/netmon.pid) \$(cat /mnt/nvme/sampler.pid) 2>/dev/null"; $SCP $ip:/mnt/nvme/net.txt $R/raw/$tag-net$i.txt; $SCP $ip:/mnt/nvme/s.csv $R/raw/$tag-node$i.csv; $NSSH $ip "du -sb /mnt/nvme/wd | cut -f1; grep -c -i panicked /mnt/nvme/wd/node.out; grep -ciE ' (WARN|ERROR) ' /mnt/nvme/wd/logs/application.log 2>/dev/null" > $R/raw/$tag-node$i.log; i=$((i+1)); done
  echo "$t0 $t1" > $R/raw/$tag-times.txt
  while IFS=, read phase bytes cl ops errs rate mean p50 p90 p99; do
    [ "$phase" = put ] || [ "$phase" = get ] || continue
    echo "$net,$arm,$size,$clients,$rep,$phase,$ops,$errs,$rate,$mean,$p50,$p90,$p99" >> $R/summary.csv
  done < $R/raw/$tag.csv
  log "$tag: $(grep -E '^(put|get),' $R/raw/$tag.csv | cut -d, -f1,4,5,6,8,9 | tr '\n' ' ') $(grep -E '^(row_n|shard_n|s3_put_n)' $R/raw/$tag-timing.txt | tr '\n' ' ')"
}
rotate() { local n=$1; shift; local a=("$@") c=$#; for j in $(seq 0 $((c-1))); do echo -n "${a[$(( (n+j) % c ))]} "; done; }
for net in $NETS; do
  bash ~/geo_netem.sh $([ $net = geo ] && echo on || echo off) >> $R/netem.log 2>&1; log "netem $net: $(tail -1 $R/netem.log)"
  for rep in $(seq 1 $REPS); do for spec in $SPECS; do IFS=: read size clients <<< "$spec"
    for arm in $(rotate $((rep-1)) $ARMS); do run $net $arm $size $clients $rep; done; done; done
  log "NET $net DONE"
done
bash ~/geo_netem.sh off >> $R/netem.log 2>&1; log "netem off: $(tail -1 $R/netem.log)"
log "OR1 DONE"
