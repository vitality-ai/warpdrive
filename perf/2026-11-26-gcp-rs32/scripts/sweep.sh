#!/usr/bin/env bash
# sweep.sh, runs on wlc, detached. S3 object size sweep on 5 x n2-standard-2 (local NVMe), RS(3,2), RocksDB metadata
# (1 GB cache), every other setting default. Two arms, alternated at every size, each on a fresh cluster:
#   base = #244 head (df476de, wd-244)    pr = feat-s3-write-path (5476e91, wd-pr)
# Per size: 60 s of new PUTs, then 60 s of GETs over them, from s3_bench on wlc, every request to node 0. Clients shrink
# as objects grow so the data in flight fits the 8 GB nodes. Node CPU/NVMe/RSS every 5 s (sampler started and stopped
# by PID), client CPU every 5 s. DRY=1: 4 KB only, 10 s phases.
R=${R:-~/sweep}; rm -rf $R; mkdir -p $R/raw; log() { echo "$(date -u +%T) $*" >> $R/progress.log; }
IPS=($(cat ~/ips.txt)); NSSH="ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -n"
SCP="scp -q -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
AK=AKIAIOSFODNN7EXAMPLE; SK=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY; PEERS=$(printf "http://%s:9710," "${IPS[@]}"); PEERS=${PEERS%,}
BASE="WARPDRIVE_META_ENGINE=rocksdb WARPDRIVE_ROCKSDB_CACHE_MB=1024 WARPDRIVE_SEGMENT_MB=64 WARPDRIVE_RECLAIM_GRACE_SECS=120 WARPDRIVE_RECLAIM_CONFIRM_SECS=60 WARPDRIVE_COMPACT_SEAL_SECS=120"
SPECS=${SPECS:-"4096:64 65536:64 1048576:32 8388608:16 67108864:4 268435456:2"}; DUR=${DUR:-60}
[ -n "$DRY" ] && { SPECS="4096:64"; DUR=10; }
for b in wd-244 wd-pr; do log "$b $(md5sum ~/bins/$b | cut -c1-8)"; for ip in "${IPS[@]}"; do $SCP ~/bins/$b $ip:bins/ || { log "ABORT scp $b $ip"; exit 1; }; done; done
log "s3_bench $(md5sum ~/bins/s3b-combo | cut -c1-8); nodes ${IPS[*]}; $BASE"
echo "arm,size,clients,phase,ops,errors,ops_per_s,mb_per_s,mean_us,p50_us,p90_us,p99_us" > $R/summary.csv
ready() { for k in $(seq 1 300); do ok=0; for ip in "${IPS[@]}"; do curl -s -m 2 -o /dev/null -w '%{http_code}' http://$ip:9710/ | grep -q '^[234]' && ok=$((ok+1)); done; [ $ok = 5 ] && return 0; sleep 1; done; return 1; }
fresh() {  # fresh <binary>
  for ip in "${IPS[@]}"; do $NSSH $ip "pkill -x wd-244; pkill -x wd-pr; pkill -x wd-q; pkill -x wd-redis; pkill -x ba; for k in \$(seq 1 60); do pgrep -x wd-244 >/dev/null || pgrep -x wd-pr >/dev/null || break; sleep 1; done; for p in 9710 10710 11710 6379; do for k in \$(seq 1 30); do ss -ltn | grep -q \":\$p \" || break; sleep 1; done; done; rm -rf /mnt/nvme/wd /mnt/nvme/ba; mkdir -p /mnt/nvme/wd; cp ~/bins/comp_log.yaml /mnt/nvme/wd/server_log.yaml" & done; wait
  for ip in "${IPS[@]}"; do $NSSH $ip "cd /mnt/nvme/wd; ulimit -n 1048576; env $BASE WARPDRIVE_SELF_ADDR=http://$ip:9710 WARPDRIVE_PEERS=$PEERS WARPDRIVE_RS_K=3 WARPDRIVE_RS_M=2 WARPDRIVE_PORT=9710 WARPDRIVE_ADMIN_ACCESS_KEY=$AK WARPDRIVE_ADMIN_SECRET_KEY=$SK setsid nohup ~/bins/$1 > node.out 2>&1 < /dev/null &"; done
  ready || return 1; sleep 5; }
for spec in $SPECS; do
  IFS=: read size clients <<< "$spec"
  for arm in base pr; do
    bin=wd-244; [ $arm = pr ] && bin=wd-pr
    fresh $bin || { log "ABORT $arm $size: cluster not ready"; exit 1; }
    tag=$arm-$size
    for ip in "${IPS[@]}"; do $NSSH $ip "rm -f /mnt/nvme/s.csv; setsid nohup bash ~/bins/soak_sampler.sh $bin /mnt/nvme/s.csv > /dev/null 2>&1 < /dev/null & echo \$! > /mnt/nvme/sampler.pid"; done
    ( while sleep 5; do echo "$(date +%s),$(top -bn1 | awk '/^%Cpu/{print 100-$8}')" >> $R/raw/$tag-client-cpu.csv; done ) & C=$!
    ~/bins/s3b-combo http://${IPS[0]}:9710 "" bench $clients $DUR $size > $R/raw/$tag.csv 2> $R/raw/$tag.err
    kill $C 2>/dev/null
    i=0; for ip in "${IPS[@]}"; do $NSSH $ip "kill \$(cat /mnt/nvme/sampler.pid) 2>/dev/null"; $SCP $ip:/mnt/nvme/s.csv $R/raw/$tag-node$i.csv; $NSSH $ip "grep -c -i panicked /mnt/nvme/wd/node.out; grep -c ' WARN \| ERROR ' /mnt/nvme/wd/logs/application.log 2>/dev/null; grep ' WARN \| ERROR ' /mnt/nvme/wd/logs/application.log 2>/dev/null | cut -c1-300 | sort | uniq -c | sort -rn | head -5" > $R/raw/$tag-node$i.log; i=$((i+1)); done
    while IFS=, read phase bytes cl ops errs rate mean p50 p90 p99; do
      [ "$phase" = put ] || [ "$phase" = get ] || continue
      echo "$arm,$size,$clients,$phase,$ops,$errs,$rate,$(echo "$rate * $size / 1048576" | bc -l | cut -c1-7),$mean,$p50,$p90,$p99" >> $R/summary.csv
    done < $R/raw/$tag.csv
    log "$arm ${size}B x $clients: $(tr '\n' ' ' < $R/raw/$tag.csv) | node warn/err lines: $(for f in $R/raw/$tag-node*.log; do sed -n 2p $f; done | tr '\n' ' ')"
  done
done
for ip in "${IPS[@]}"; do $NSSH $ip "pkill -x wd-244; pkill -x wd-pr"; done
log "ALL DONE"
