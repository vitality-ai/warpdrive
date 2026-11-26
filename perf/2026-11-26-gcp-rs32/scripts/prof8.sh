#!/usr/bin/env bash
# prof8.sh, runs on wlc after sweep.sh. Where does a large object's time go? PR build (wd-pr), RS(3,2) on 5 nodes,
# RocksDB, 8 MB objects x 16 clients from s3_bench on wlc to node 0: 90 s PUT then 90 s GET.
# In each phase at +25 s: perf record (dwarf) 15 s on node0, node1 and the client; then perf stat + pidstat 10 s on node0;
# network bytes (/proc/net/dev) and disk on node0, node1 and the client over the whole run, every 2 s.
SIZE=${SIZE:-8388608}; CL=${CL:-16}; DUR=${DUR:-90}
P=~/prof8; rm -rf $P; mkdir -p $P; log() { echo "$(date -u +%T) $*" >> $P/progress.log; }
IPS=($(cat ~/ips.txt)); NSSH="ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -n"
SCP="scp -q -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
AK=AKIAIOSFODNN7EXAMPLE; SK=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY; PEERS=$(printf "http://%s:9710," "${IPS[@]}"); PEERS=${PEERS%,}
BASE="WARPDRIVE_META_ENGINE=rocksdb WARPDRIVE_ROCKSDB_CACHE_MB=1024 WARPDRIVE_SEGMENT_MB=64 WARPDRIVE_RECLAIM_GRACE_SECS=120 WARPDRIVE_RECLAIM_CONFIRM_SECS=60 WARPDRIVE_COMPACT_SEAL_SECS=120"
which perf > /dev/null || sudo DEBIAN_FRONTEND=noninteractive apt-get -qq install -y linux-tools-$(uname -r) linux-tools-gcp sysstat > /dev/null 2>&1
for i in 0 1; do $SCP ~/FlameGraph/stackcollapse-perf.pl ~/FlameGraph/flamegraph.pl ~/netmon.sh ${IPS[$i]}:bins/; done
for ip in "${IPS[@]}"; do $NSSH $ip "pkill -x wd-244; pkill -x wd-pr; pkill -x wd-q; for p in 9710 10710 11710; do for k in \$(seq 1 30); do ss -ltn | grep -q \":\$p \" || break; sleep 1; done; done; rm -rf /mnt/nvme/wd /mnt/nvme/prof /tmp/net.csv; mkdir -p /mnt/nvme/wd /mnt/nvme/prof; cp ~/bins/comp_log.yaml /mnt/nvme/wd/server_log.yaml" & done; wait
for ip in "${IPS[@]}"; do $NSSH $ip "cd /mnt/nvme/wd; ulimit -n 1048576; env $BASE WARPDRIVE_SELF_ADDR=http://$ip:9710 WARPDRIVE_PEERS=$PEERS WARPDRIVE_RS_K=3 WARPDRIVE_RS_M=2 WARPDRIVE_PORT=9710 WARPDRIVE_ADMIN_ACCESS_KEY=$AK WARPDRIVE_ADMIN_SECRET_KEY=$SK setsid nohup ~/bins/wd-pr > node.out 2>&1 < /dev/null &"; done
for k in $(seq 1 120); do ok=0; for ip in "${IPS[@]}"; do curl -s -m 2 -o /dev/null -w '%{http_code}' http://$ip:9710/ | grep -q '^[234]' && ok=$((ok+1)); done; [ $ok = 5 ] && break; sleep 1; done
[ $ok = 5 ] || { log "ABORT cluster"; exit 1; }
sleep 5
for i in 0 1; do $NSSH ${IPS[$i]} "rm -f /tmp/net.csv; setsid nohup bash ~/bins/netmon.sh /tmp/net.csv > /dev/null 2>&1 < /dev/null & echo \$! > /tmp/netmon.pid"; done
rm -f /tmp/net.csv; setsid nohup bash ~/netmon.sh /tmp/net.csv > /dev/null 2>&1 < /dev/null & NM=$!
T0=$(date +%s); log "start: $SIZE B x $CL clients, $DUR s PUT then $DUR s GET (wd-pr $(md5sum ~/bins/wd-pr | cut -c1-8))"
~/bins/s3b-combo http://${IPS[0]}:9710 "" bench $CL $DUR $SIZE > $P/bench.csv 2> $P/bench.err &
B=$!
capture() {  # capture <phase> <offset>
  ph=$1; sleep $(( T0 + $2 + 25 - $(date +%s) ))
  for i in 0 1; do $NSSH ${IPS[$i]} "cd /mnt/nvme/prof; setsid nohup sudo perf record -F 99 --call-graph dwarf,16384 -o $ph-node$i.data -p \$(pgrep -x wd-pr) -- sleep 15 > /dev/null 2>&1 < /dev/null &"; done
  sudo perf record -F 99 --call-graph dwarf,16384 -o $P/$ph-client.data -p $(pgrep -x s3b-combo) -- sleep 15 > /dev/null 2>&1
  echo "$ph stat $(date +%s)" >> $P/windows.txt
  $NSSH ${IPS[0]} "cd /mnt/nvme/prof; pid=\$(pgrep -x wd-pr); sudo perf stat -e task-clock,context-switches,cpu-migrations,syscalls:sys_enter_write,syscalls:sys_enter_pwrite64,syscalls:sys_enter_sendto,syscalls:sys_enter_recvfrom,syscalls:sys_enter_fdatasync,syscalls:sys_enter_fsync,syscalls:sys_enter_futex -p \$pid -- sleep 10 2> $ph-perfstat.txt & pidstat -u -w -t -h -p \$pid 10 1 > $ph-pidstat.txt; wait"
  log "$ph captured"; }
capture put 0
capture get $DUR
wait $B
log "bench: $(tr '\n' ' ' < $P/bench.csv) $(head -c 300 $P/bench.err)"
kill $NM; cp /tmp/net.csv $P/client-net.csv
for i in 0 1; do $NSSH ${IPS[$i]} "kill \$(cat /tmp/netmon.pid)"; $SCP ${IPS[$i]}:/tmp/net.csv $P/node$i-net.csv; done
for ph in put get; do
  for i in 0 1; do
    $NSSH ${IPS[$i]} "cd /mnt/nvme/prof; sudo perf report -i $ph-node$i.data --no-children --sort dso --stdio -g none 2>/dev/null | grep -v '^#' | grep . | head -15 > $ph-node$i.dso.txt; sudo perf report -i $ph-node$i.data --no-children --sort symbol --stdio -g none 2>/dev/null | grep -v '^#' | grep . | head -60 > $ph-node$i.flat.txt; sudo perf report -i $ph-node$i.data --children --sort symbol --stdio -g none 2>/dev/null | grep -v '^#' | grep . | head -150 > $ph-node$i.children.txt; sudo perf script -i $ph-node$i.data 2>/dev/null | perl ~/bins/stackcollapse-perf.pl > $ph-node$i.folded 2>/dev/null; perl ~/bins/flamegraph.pl $ph-node$i.folded > $ph-node$i.svg 2>/dev/null; true"
    $SCP "${IPS[$i]}:/mnt/nvme/prof/$ph-node$i.*.txt" "${IPS[$i]}:/mnt/nvme/prof/$ph-node$i.svg" $P/
  done
  $SCP "${IPS[0]}:/mnt/nvme/prof/$ph-perfstat.txt" "${IPS[0]}:/mnt/nvme/prof/$ph-pidstat.txt" $P/
  sudo perf report -i $P/$ph-client.data --no-children --sort symbol --stdio -g none 2>/dev/null | grep -v '^#' | grep . | head -40 > $P/$ph-client.flat.txt
  sudo perf report -i $P/$ph-client.data --no-children --sort dso --stdio -g none 2>/dev/null | grep -v '^#' | grep . | head -10 > $P/$ph-client.dso.txt
done
for ip in "${IPS[@]}"; do $NSSH $ip "pkill -x wd-pr"; done
log "ALL DONE"
