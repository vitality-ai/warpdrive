#!/usr/bin/env bash
# zs.sh, on wlc, detached: zstd blob compression off vs on (same binary, WARPDRIVE_BLOB_COMPRESSION), 5 x n2-standard-2,
# RS(3,2), RocksDB. Payloads: text (JSON lines) and random. Per run: 60 s PUT then 60 s GET from s3_bench on wlc to node 0.
# Every node: NIC/NVMe counters every 2 s (netmon.sh) and CPU every 5 s (soak_sampler.sh).
source ~/.cargo/env; R=~/zs; rm -rf $R; mkdir -p $R/raw; exec > $R/driver.log 2>&1; log() { echo "$(date -u +%T) $*" | tee -a $R/progress.log; }
log build
rm -rf ~/src-z; mkdir -p ~/src-z; tar xzf ~/src-z.tgz -C ~/src-z
(cd ~/src-z/server && CARGO_TARGET_DIR=~/tgt-redis cargo build --release -q --bin warp_drive --bin s3_bench) && cp ~/tgt-redis/release/warp_drive ~/bins/wd-z && cp ~/tgt-redis/release/s3_bench ~/bins/s3b-z || { log "BUILD FAILED"; exit 1; }
log "wd-z $(md5sum ~/bins/wd-z | cut -c1-8) s3b-z $(md5sum ~/bins/s3b-z | cut -c1-8)"
IPS=($(cat ~/ips.txt)); NSSH="ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -n"
SCP="scp -q -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
AK=AKIAIOSFODNN7EXAMPLE; SK=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY; PEERS=$(printf "http://%s:9710," "${IPS[@]}"); PEERS=${PEERS%,}
BASE="WARPDRIVE_META_ENGINE=rocksdb WARPDRIVE_ROCKSDB_CACHE_MB=1024 WARPDRIVE_SEGMENT_MB=64 WARPDRIVE_RECLAIM_GRACE_SECS=120 WARPDRIVE_RECLAIM_CONFIRM_SECS=60 WARPDRIVE_COMPACT_SEAL_SECS=120"
DUR=${DUR:-60}; SPECS=${SPECS:-"65536:64 1048576:32 8388608:16"}; DATAS=${DATAS:-"text random"}
for ip in "${IPS[@]}"; do $SCP ~/bins/wd-z ~/netmon.sh $ip:bins/ || { log "ABORT scp $ip"; exit 1; }; done
ready() { for k in $(seq 1 300); do ok=0; for ip in "${IPS[@]}"; do curl -s -m 2 -o /dev/null -w '%{http_code}' http://$ip:9710/ | grep -q '^[234]' && ok=$((ok+1)); done; [ $ok = 5 ] && return 0; sleep 1; done; return 1; }
fresh() {  # fresh <extra env>
  for ip in "${IPS[@]}"; do $NSSH $ip "for b in wd-244 wd-pr wd-prb wd-fe wd-z wd-redis ba; do pkill -x \$b; done; for k in \$(seq 1 60); do pgrep -x wd-z >/dev/null || pgrep -x wd-fe >/dev/null || pgrep -x wd-prb >/dev/null || break; sleep 1; done; for p in 9710 10710 11710; do for k in \$(seq 1 30); do ss -ltn | grep -q \":\$p \" || break; sleep 1; done; done; rm -rf /mnt/nvme/wd; mkdir -p /mnt/nvme/wd; cp ~/bins/comp_log.yaml /mnt/nvme/wd/server_log.yaml" & done; wait
  for ip in "${IPS[@]}"; do $NSSH $ip "cd /mnt/nvme/wd; ulimit -n 1048576; env $BASE $1 WARPDRIVE_SELF_ADDR=http://$ip:9710 WARPDRIVE_PEERS=$PEERS WARPDRIVE_RS_K=3 WARPDRIVE_RS_M=2 WARPDRIVE_PORT=9710 WARPDRIVE_ADMIN_ACCESS_KEY=$AK WARPDRIVE_ADMIN_SECRET_KEY=$SK setsid nohup ~/bins/wd-z > node.out 2>&1 < /dev/null &"; done
  ready || return 1; sleep 5; }
echo "data,arm,size,clients,phase,ops,errors,ops_per_s,mean_us,p50_us,p90_us,p99_us" > $R/summary.csv
for data in $DATAS; do for spec in $SPECS; do IFS=: read size clients <<< "$spec"; for arm in off zstd; do
  env=""; [ $arm = zstd ] && env="WARPDRIVE_BLOB_COMPRESSION=zstd"
  fresh "$env" || { log "ABORT $data $arm $size: cluster not ready"; exit 1; }
  tag=$data-$arm-$size
  for ip in "${IPS[@]}"; do $NSSH $ip "rm -f /mnt/nvme/net.txt /mnt/nvme/s.csv; setsid nohup bash ~/bins/netmon.sh /mnt/nvme/net.txt > /dev/null 2>&1 < /dev/null & echo \$! > /mnt/nvme/netmon.pid; setsid nohup bash ~/bins/soak_sampler.sh wd-z /mnt/nvme/s.csv > /dev/null 2>&1 < /dev/null & echo \$! > /mnt/nvme/sampler.pid"; done
  sleep 3; t0=$(date +%s)
  S3B_DATA=$data ~/bins/s3b-z http://${IPS[0]}:9710 "" bench $clients $DUR $size > $R/raw/$tag.csv 2> $R/raw/$tag.err
  t1=$(date +%s); sleep 3
  i=0; for ip in "${IPS[@]}"; do $NSSH $ip "kill \$(cat /mnt/nvme/netmon.pid) \$(cat /mnt/nvme/sampler.pid) 2>/dev/null"; $SCP $ip:/mnt/nvme/net.txt $R/raw/$tag-net$i.txt; $SCP $ip:/mnt/nvme/s.csv $R/raw/$tag-node$i.csv; $NSSH $ip "du -sb /mnt/nvme/wd | cut -f1; grep -c -i panicked /mnt/nvme/wd/node.out" > $R/raw/$tag-node$i.log; i=$((i+1)); done
  echo "$t0 $t1" > $R/raw/$tag-times.txt
  while IFS=, read phase bytes cl ops errs rate mean p50 p90 p99; do
    [ "$phase" = put ] || [ "$phase" = get ] || continue
    echo "$data,$arm,$size,$clients,$phase,$ops,$errs,$rate,$mean,$p50,$p90,$p99" >> $R/summary.csv
  done < $R/raw/$tag.csv
  log "$tag: $(tr '\n' ' ' < $R/raw/$tag.csv) disk: $(for f in $R/raw/$tag-node*.log; do sed -n 1p $f; done | tr '\n' ' ')"
done; done; done
log "ZS DONE"
