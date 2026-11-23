#!/usr/bin/env bash
# Runs on wlc: the last experiment before the release. Bitcask against RocksDB with #238 on 5 nodes (n2-standard-2, local
# NVMe), RS(3,2), each arm on a fresh cluster, with increasing object sizes: for each size, 60 s of new PUTs, then 60 s of
# GETs over them. Clients shrink as objects grow so the data in flight fits the 8 GB nodes. Node CPU, RSS and client CPU
# are sampled every 5 s. Prebuilt binaries, no rebuild:
#   bitcask = wd-final3 (#224 at 766060b, Bitcask default), combined = wd-combo (#238 + #237, ce20fc3, RocksDB with a
#   1 GB cache).
# Started by hand after reconf32.sh.
R=~/sanity32; rm -rf $R; mkdir -p $R; log() { echo "$(date -u +%T) $*" >> $R/progress.log; }
IPS=($(cat ~/ips.txt)); NSSH="ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -n"
AK=AKIAIOSFODNN7EXAMPLE; SK=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY; PEERS=$(printf "http://%s:9710," "${IPS[@]}"); PEERS=${PEERS%,}
BASE="WARPDRIVE_SEGMENT_MB=64 WARPDRIVE_RECLAIM_GRACE_SECS=120 WARPDRIVE_RECLAIM_CONFIRM_SECS=60 WARPDRIVE_COMPACT_SEAL_SECS=120 WARPDRIVE_ROCKSDB_CACHE_MB=1024"
ready() { for k in $(seq 1 300); do ok=0; for i in 0 1 2 3 4; do curl -s -m 2 -o /dev/null -w '%{http_code}' http://${IPS[$i]}:9710/ | grep -q '^[234]' && ok=$((ok+1)); done; [ $ok = 5 ] && return 0; sleep 1; done; return 1; }
for arm in bitcask:wd-final3: combined:wd-combo:WARPDRIVE_META_ENGINE=rocksdb; do
  IFS=: read name bin extra <<< "$arm"
  for i in 0 1 2 3 4; do $NSSH ${IPS[$i]} "pkill -x wd-final3; pkill -x wd-rocks; pkill -x wd-combo; for k in \$(seq 1 60); do pgrep -x $bin >/dev/null || pgrep -x wd-combo >/dev/null || pgrep -x wd-rocks >/dev/null || pgrep -x wd-final3 >/dev/null || break; sleep 1; done; for p in 9710 10710 11710; do for k in \$(seq 1 30); do ss -ltn | grep -q \":\$p \" || break; sleep 1; done; done; rm -rf /mnt/nvme/wd; mkdir -p /mnt/nvme/wd; cp ~/bins/comp_log.yaml /mnt/nvme/wd/server_log.yaml"; done
  for i in 0 1 2 3 4; do $NSSH ${IPS[$i]} "cd /mnt/nvme/wd; ulimit -n 1048576; env $BASE $extra WARPDRIVE_SELF_ADDR=http://${IPS[$i]}:9710 WARPDRIVE_PEERS=$PEERS WARPDRIVE_RS_K=3 WARPDRIVE_RS_M=2 WARPDRIVE_PORT=9710 WARPDRIVE_ADMIN_ACCESS_KEY=$AK WARPDRIVE_ADMIN_SECRET_KEY=$SK setsid nohup ~/bins/$bin > node.out 2>&1 < /dev/null &"; done
  ready || { log "ABORT $name not ready"; continue; }
  sleep 5
  for i in 0 1 2 3 4; do $NSSH ${IPS[$i]} "pkill -f '[s]oak_sampler.sh'; setsid nohup bash ~/bins/soak_sampler.sh $bin /mnt/nvme/$name.csv > /dev/null 2>&1 < /dev/null &"; done
  ( while sleep 5; do echo "$(date +%s),$(top -bn1 | awk '/^%Cpu/{print 100-$8}')" >> $R/$name-client-cpu.csv; done ) & C=$!
  log "$name: engine dirs on a node: $($NSSH ${IPS[1]} "ls -d /mnt/nvme/wd/*.rocks 2>/dev/null | wc -l") rocks"
  for spec in 4096:64 65536:64 1048576:32 8388608:16 67108864:4; do
    IFS=: read size clients <<< "$spec"
    ~/bins/s3b-combo http://${IPS[0]}:9710 "" bench $clients 60 $size > $R/$name-$size.csv 2> $R/$name-$size.err
    log "$name $size B x $clients clients: $(tr '\n' ' ' < $R/$name-$size.csv)"
  done
  kill $C 2>/dev/null
  for i in 0 1 2 3 4; do $NSSH ${IPS[$i]} "pkill -f '[s]oak_sampler.sh'"; scp -q -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR ${IPS[$i]}:/mnt/nvme/$name.csv $R/$name-node$i.csv; done
done
log "ALL DONE"
