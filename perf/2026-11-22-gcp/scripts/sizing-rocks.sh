#!/usr/bin/env bash
# Runs on wlc. The sizing run again with WARPDRIVE_META_ENGINE=rocksdb (feat-rocksdb-meta-engine), to compare with the
# Bitcask sizing run on the same VMs: RS(2,1) on wln-0..2, new 4 KB objects at 64 clients in slices (each a PUT phase then
# a GET phase of the same length), then a restart and idle RSS, startup time, metadata and segment sizes per node.
# A guard stops the load past 6 GB RSS on any node (nodes 1-2 have 8 GB).
CACHE_MB=${CACHE_MB:-1024}
until grep -q "ALL DONE\|ABORT" ~/kvb2/progress.log 2>/dev/null; do sleep 10; done
R=~/sizing-rocks; rm -rf $R; mkdir -p $R; log() { echo "$(date -u +%T) $*" >> $R/progress.log; }
IPS=($(cat ~/ips.txt)); NSSH="ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -n"
AK=AKIAIOSFODNN7EXAMPLE; SK=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY; PEERS=$(printf "http://%s:9710," "${IPS[@]}"); PEERS=${PEERS%,}
CENV="WARPDRIVE_META_ENGINE=rocksdb WARPDRIVE_ROCKSDB_CACHE_MB=$CACHE_MB WARPDRIVE_SEGMENT_MB=64 WARPDRIVE_RECLAIM_GRACE_SECS=120 WARPDRIVE_RECLAIM_CONFIRM_SECS=60 WARPDRIVE_COMPACT_SEAL_SECS=120"
. ~/.cargo/env; rm -rf ~/src-rocks; mkdir -p ~/src-rocks; tar xzf ~/src-rocks.tgz -C ~/src-rocks 2>/dev/null
(cd ~/src-rocks/server && CARGO_TARGET_DIR=$HOME/tgt-rocks cargo build --release --bin warp_drive 2>&1 | tail -1) > $R/build.log
grep -q Finished $R/build.log || { log "ABORT build failed"; exit 1; }
cp ~/tgt-rocks/release/warp_drive ~/bins/wd-rocks
for ip in "${IPS[@]}"; do $NSSH $ip "mountpoint -q /mnt/nvme" || { log "ABORT no nvme $ip"; exit 1; }; scp -q -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR ~/bins/wd-rocks $ip:~/bins/; done
log "bin: $(cat ~/src-rocks/COMMIT) $(sha256sum ~/bins/wd-rocks | cut -c1-16) engine=rocksdb cache_mb=$CACHE_MB"
launch() { i=$1; $NSSH ${IPS[$i]} "cd /mnt/nvme/wd; ulimit -n 1048576; env $CENV WARPDRIVE_SELF_ADDR=http://${IPS[$i]}:9710 WARPDRIVE_PEERS=$PEERS WARPDRIVE_RS_K=2 WARPDRIVE_RS_M=1 WARPDRIVE_PORT=9710 WARPDRIVE_ADMIN_ACCESS_KEY=$AK WARPDRIVE_ADMIN_SECRET_KEY=$SK setsid nohup ~/bins/wd-rocks >> node.out 2>&1 < /dev/null &"; }
stop_all() { for i in 0 1 2; do $NSSH ${IPS[$i]} "pkill -x wd-rocks; for k in \$(seq 1 60); do pgrep -x wd-rocks >/dev/null || break; sleep 1; done; for p in 9710 10710 11710; do for k in \$(seq 1 30); do ss -ltn | grep -q \":\$p \" || break; sleep 1; done; done"; done; }
ready() { for k in $(seq 1 1800); do ok=0; for i in 0 1 2; do curl -s -m 2 -o /dev/null -w '%{http_code}' http://${IPS[$i]}:9710/ | grep -q '^[234]' && ok=$((ok+1)); done; [ $ok = 3 ] && return 0; sleep 1; done; return 1; }
# Fresh cluster.
for i in 0 1 2; do $NSSH ${IPS[$i]} "pkill -x wd-final3; pkill -x wd-rocks; pkill -x wd-final2; pkill -x kvbench; pkill -f '[s]ampler.sh'; sleep 1; rm -rf /mnt/nvme/wd /mnt/nvme/soak.csv; mkdir -p /mnt/nvme/wd; cp ~/bins/comp_log.yaml /mnt/nvme/wd/server_log.yaml"; done
stop_all
for i in 0 1 2; do launch $i; done; ready || { log "ABORT cluster not ready"; exit 1; }
echo "slice,objects_total,node,rss_kb_idle,startup_s,meta_kb,seg_kb,shard_index_kb" > $R/sizing.csv
echo "slice,node,unix_s,rss_kb" > $R/load_rss.csv
total=0; slice=0
for secs in 500 1000; do
  slice=$((slice+1)); log "slice $slice: loading for $secs s"
  # Guard: sample RSS every 10 s, stop the load past 12 GB.
  ( while sleep 10; do for i in 0 1 2; do r=$($NSSH ${IPS[$i]} "ps -o rss= -C wd-rocks | head -1" | tr -d ' '); echo "$slice,$i,$(date +%s),$r" >> $R/load_rss.csv; [ -n "$r" ] && [ "$r" -gt 6291456 ] && { echo "$(date -u +%T) GUARD node $i rss ${r} KB" >> $R/progress.log; pkill -x s3b-comp; }; done; done ) & G=$!
  ~/bins/s3b-comp http://${IPS[0]}:9710 "" bench 64 $secs 4096 > $R/bench$slice.csv 2> $R/bench$slice.err
  kill $G 2>/dev/null; wait $G 2>/dev/null
  n=$(cut -d, -f4 $R/bench$slice.csv | head -1); e=$(cut -d, -f5 $R/bench$slice.csv | head -1); total=$((total+n))
  log "slice $slice: $n PUTs, $e errors, total $total: $(head -1 $R/bench$slice.csv)"
  sleep 30; stop_all
  t0=$(date +%s); for i in 0 1 2; do launch $i; done; ready || { log "ABORT not ready after restart"; exit 1; }; up=$(( $(date +%s) - t0 ))
  sleep 60
  for i in 0 1 2; do
    m=$($NSSH ${IPS[$i]} "echo \$(ps -o rss= -C wd-rocks | head -1),\$(du -sk /mnt/nvme/wd/*.log /mnt/nvme/wd/*.rocks 2>/dev/null | awk '{s+=\$1} END{print s}'),\$(du -sk /mnt/nvme/wd/storage 2>/dev/null | cut -f1),\$(du -sk /mnt/nvme/wd/shard_meta.rocks 2>/dev/null | cut -f1)" | tr -d ' ')
    IFS=, read rss meta seg sidx <<< "$m"
    echo "$slice,$total,$i,$rss,$up,$meta,$seg,$sidx" >> $R/sizing.csv
  done
  log "slice $slice measured: $(grep "^$slice," $R/sizing.csv | tr '\n' ' ')"
  grep -q GUARD $R/progress.log && { log "stopping: memory guard"; break; }
done
for i in 0 1 2; do $NSSH ${IPS[$i]} "ls -la /mnt/nvme/wd/ /mnt/nvme/wd/*.log; grep -c -i panicked /mnt/nvme/wd/node.out" > $R/node$i.ls; done
log "ALL DONE"
