#!/usr/bin/env bash
# Runs on wlc. The sizing run on RocksDB plus reads carrying placements (feat-rocksdb-meta-engine merged with
# feat-read-with-placement, local build): no per-object state on any node. To compare with the
# Bitcask and RocksDB-only sizing runs on the same VMs: RS(2,1) on wln-0..2, new 4 KB objects at 64 clients, about 35
# minutes: PUT-only loads each followed by a restart and idle RSS, startup time, metadata and segment sizes per node, and
# two PUT+GET perf probes (at about 1.6 M and 4.6 M objects).
# A guard stops the load past 6 GB RSS on any node (nodes 1-2 have 8 GB).
CACHE_MB=${CACHE_MB:-1024}
R=~/sizing-combo; rm -rf $R; mkdir -p $R; log() { echo "$(date -u +%T) $*" >> $R/progress.log; }
IPS=($(cat ~/ips.txt)); NSSH="ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -n"
AK=AKIAIOSFODNN7EXAMPLE; SK=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY; PEERS=$(printf "http://%s:9710," "${IPS[@]}"); PEERS=${PEERS%,}
CENV="WARPDRIVE_META_ENGINE=rocksdb WARPDRIVE_ROCKSDB_CACHE_MB=$CACHE_MB WARPDRIVE_SEGMENT_MB=64 WARPDRIVE_RECLAIM_GRACE_SECS=120 WARPDRIVE_RECLAIM_CONFIRM_SECS=60 WARPDRIVE_COMPACT_SEAL_SECS=120"
. ~/.cargo/env
# Source from GitHub (no upload from the Mac): #238's head merged with #237's head, plus S3_BENCH_PUT_ONLY in s3_bench.
rm -rf ~/src-combo; git clone -q https://github.com/vitality-ai/warpdrive.git ~/src-combo || { log "ABORT clone failed"; exit 1; }
cd ~/src-combo && git checkout -q 58ecf642f4c3c8abd8d938ce10b089ac2bafb04e && git -c user.name=combo -c user.email=combo@local merge -q --no-edit 10bec1a || { log "ABORT merge failed"; exit 1; }
python3 - <<'PY'
p='server/src/bin/s3_bench.rs'; s=open(p).read()
a='    report("put", payload_bytes, concurrency, duration, put);\n'
assert s.count(a)==1
s=s.replace(a, a+'    if std::env::var("S3_BENCH_PUT_ONLY").is_ok() {\n        return;\n    }\n')
open(p,'w').write(s)
PY
echo "$(git rev-parse HEAD) = 58ecf64 (#238) + 10bec1a (#237) + s3_bench PUT-only" > ~/src-combo/COMMIT; cd ~
(cd ~/src-combo/server && CARGO_TARGET_DIR=$HOME/tgt-combo cargo build --release --bin warp_drive 2>&1 | tail -1) > $R/build.log
grep -q Finished $R/build.log || { log "ABORT build failed"; exit 1; }
cp ~/tgt-combo/release/warp_drive ~/bins/wd-combo
(cd ~/src-combo/server && CARGO_TARGET_DIR=$HOME/tgt-combo cargo build --release --bin s3_bench 2>&1 | tail -1) >> $R/build.log; cp ~/tgt-combo/release/s3_bench ~/bins/s3b-combo
for ip in "${IPS[@]}"; do $NSSH $ip "mountpoint -q /mnt/nvme" || { log "ABORT no nvme $ip"; exit 1; }; scp -q -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR ~/bins/wd-combo $ip:~/bins/; done
log "bin: $(cat ~/src-combo/COMMIT) $(sha256sum ~/bins/wd-combo | cut -c1-16) engine=rocksdb cache_mb=$CACHE_MB"
launch() { i=$1; $NSSH ${IPS[$i]} "cd /mnt/nvme/wd; ulimit -n 1048576; env $CENV WARPDRIVE_SELF_ADDR=http://${IPS[$i]}:9710 WARPDRIVE_PEERS=$PEERS WARPDRIVE_RS_K=2 WARPDRIVE_RS_M=1 WARPDRIVE_PORT=9710 WARPDRIVE_ADMIN_ACCESS_KEY=$AK WARPDRIVE_ADMIN_SECRET_KEY=$SK setsid nohup ~/bins/wd-combo >> node.out 2>&1 < /dev/null &"; }
stop_all() { for i in 0 1 2; do $NSSH ${IPS[$i]} "pkill -x wd-combo; for k in \$(seq 1 60); do pgrep -x wd-combo >/dev/null || break; sleep 1; done; for p in 9710 10710 11710; do for k in \$(seq 1 30); do ss -ltn | grep -q \":\$p \" || break; sleep 1; done; done"; done; }
ready() { for k in $(seq 1 1800); do ok=0; for i in 0 1 2; do curl -s -m 2 -o /dev/null -w '%{http_code}' http://${IPS[$i]}:9710/ | grep -q '^[234]' && ok=$((ok+1)); done; [ $ok = 3 ] && return 0; sleep 1; done; return 1; }
# Fresh cluster.
for i in 0 1 2; do $NSSH ${IPS[$i]} "pkill -x wd-final3; pkill -x wd-rocks; pkill -x wd-combo; pkill -x wd-final2; pkill -x kvbench; pkill -f '[s]ampler.sh'; sleep 1; rm -rf /mnt/nvme/wd /mnt/nvme/soak.csv; mkdir -p /mnt/nvme/wd; cp ~/bins/comp_log.yaml /mnt/nvme/wd/server_log.yaml"; done
stop_all
for i in 0 1 2; do launch $i; done; ready || { log "ABORT cluster not ready"; exit 1; }
echo "slice,objects_total,node,rss_kb_idle,startup_s,meta_kb,seg_kb,shard_index_kb" > $R/sizing.csv
echo "slice,node,unix_s,rss_kb" > $R/load_rss.csv
total=0; slice=0
# put:N loads new objects for N s (PUT only), then restarts every node and measures it idle. probe:N is the perf probe: a
# PUT phase of N s then a GET phase of N s over the keys it wrote, comparable with the Bitcask and RocksDB-only runs.
for spec in put:500 probe:120 put:500 put:400 probe:120; do
  kind=${spec%%:*}; secs=${spec#*:}
  slice=$((slice+1)); log "slice $slice: $kind for $secs s"
  # Guard: sample RSS every 10 s, stop the load past 6 GB.
  ( while sleep 10; do for i in 0 1 2; do r=$($NSSH ${IPS[$i]} "ps -o rss= -C wd-combo | head -1" | tr -d ' '); echo "$slice,$i,$(date +%s),$r" >> $R/load_rss.csv; [ -n "$r" ] && [ "$r" -gt 6291456 ] && { echo "$(date -u +%T) GUARD node $i rss ${r} KB" >> $R/progress.log; pkill -x s3b-combo; }; done; done ) & G=$!
  if [ $kind = put ]; then
    S3_BENCH_PUT_ONLY=1 ~/bins/s3b-combo http://${IPS[0]}:9710 "" bench 64 $secs 4096 > $R/bench$slice.csv 2> $R/bench$slice.err
  else
    ~/bins/s3b-combo http://${IPS[0]}:9710 "" bench 64 $secs 4096 > $R/bench$slice.csv 2> $R/bench$slice.err
  fi
  kill $G 2>/dev/null; wait $G 2>/dev/null
  n=$(cut -d, -f4 $R/bench$slice.csv | head -1); total=$((total+n))
  log "slice $slice ($kind): total $total: $(tr '\n' ' ' < $R/bench$slice.csv)"
  grep -q GUARD $R/progress.log && { log "stopping: memory guard"; break; }
  [ $kind = put ] || continue
  sleep 30; stop_all
  t0=$(date +%s); for i in 0 1 2; do launch $i; done; ready || { log "ABORT not ready after restart"; exit 1; }; up=$(( $(date +%s) - t0 ))
  sleep 60
  for i in 0 1 2; do
    m=$($NSSH ${IPS[$i]} "echo \$(ps -o rss= -C wd-combo | head -1),\$(du -sk /mnt/nvme/wd/*.log /mnt/nvme/wd/*.rocks 2>/dev/null | awk '{s+=\$1} END{print s}'),\$(du -sk /mnt/nvme/wd/storage 2>/dev/null | cut -f1),\$(du -sk /mnt/nvme/wd/shard_meta.rocks 2>/dev/null | cut -f1)" | tr -d ' ')
    IFS=, read rss meta seg sidx <<< "$m"
    echo "$slice,$total,$i,$rss,$up,$meta,$seg,$sidx" >> $R/sizing.csv
  done
  log "slice $slice measured: $(grep "^$slice," $R/sizing.csv | tr '\n' ' ')"
done
for i in 0 1 2; do $NSSH ${IPS[$i]} "ls -la /mnt/nvme/wd/ /mnt/nvme/wd/*.log; grep -c -i panicked /mnt/nvme/wd/node.out" > $R/node$i.ls; done
log "ALL DONE"
