#!/usr/bin/env bash
# Runs on wlc after seg.sh. RFC 6 compaction under a steady overwrite load, on the final #224 build.
# RS(2,1) on wln-0..2, 64 MB segments, sweep every 60 s, grace 120 s. Workloads: 100,000 keys of 4 KB at 128
# clients, 1,000 keys of 1 MB at 32 clients, 20 keys of 256 MB at 2 clients (shards 2x a segment);
# overwritten for DUR s, then read back for DUR s.
# Arms: new (44a4055: punch + copy_file_range), old (af69266: compact only, read/write copy), off (control).
until grep -q "ALL DONE\|ABORT" ~/seg/progress.log 2>/dev/null; do sleep 20; done
. $HOME/.cargo/env
R=~/comp; rm -rf $R; mkdir -p $R; log() { echo "$(date -u +%T) $*" >> $R/progress.log; }
log "building"
for v in comp compold; do rm -rf ~/src-$v; mkdir -p ~/src-$v; tar xzf ~/src-$v.tgz -C ~/src-$v 2>/dev/null
  (cd ~/src-$v/server && CARGO_TARGET_DIR=$HOME/tgt-comp cargo build --release --bin warp_drive --bin s3_bench 2>&1 | tail -1) >> $R/build.log
  cp ~/tgt-comp/release/warp_drive ~/bins/wd-$v; [ $v = comp ] && cp ~/tgt-comp/release/s3_bench ~/bins/s3b-comp
  log "built $v from $(cat ~/src-$v/COMMIT): $(sha256sum ~/bins/wd-$v | cut -c1-16)"; done
[ $(grep -c Finished $R/build.log) = 2 ] || { log "ABORT build failed"; exit 1; }
IPS=($(cat ~/ips.txt)); NSSH="ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -n"
SCP="scp -q -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
for ip in "${IPS[@]}"; do $NSSH $ip "mountpoint -q /mnt/nvme" || { log "ABORT no nvme $ip"; exit 1; }; $SCP ~/bins/wd-comp ~/bins/wd-compold ~/comp_sampler.sh ~/comp_log.yaml $ip:~/bins/; done
AK=AKIAIOSFODNN7EXAMPLE; SK=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY; PEERS=$(printf "http://%s:9710," "${IPS[@]}"); PEERS=${PEERS%,}
DUR=${DUR:-600}
CENV="WARPDRIVE_SEGMENT_MB=64 WARPDRIVE_RECLAIM_INTERVAL_SECS=60 WARPDRIVE_RECLAIM_GRACE_SECS=120 WARPDRIVE_RECLAIM_CONFIRM_SECS=60 WARPDRIVE_COMPACT_SEAL_SECS=120"
for W in 4096:100000:128:new,old,off 1048576:1000:32:new,old,off 268435456:20:2:new,off; do
SIZE=${W%%:*}; REST=${W#*:}; KEYS=${REST%%:*}; REST=${REST#*:}; CONC=${REST%%:*}; ARMS=${REST#*:}
for ARM in ${ARMS//,/ }; do
  D=$R/${SIZE}_$ARM; mkdir -p $D; ENVX=""; BIN=wd-comp; [ $ARM = off ] && ENVX="WARPDRIVE_COMPACT=off"; [ $ARM = old ] && BIN=wd-compold
  for i in 0 1 2; do $NSSH ${IPS[$i]} "pkill -x wd-comp; pkill -x wd-compold; pkill -x wd-seg; pkill -x wd-segbase; pkill -x wd-main; pkill -x wd-as; pkill -x wd-ar; pkill -f '[s]ampler.sh'; sleep 1; rm -rf /mnt/nvme/wd /mnt/nvme/comp.csv"; done
  for i in 0 1 2; do for p in 9710 10710 11710; do $NSSH ${IPS[$i]} "for k in \$(seq 1 30); do ss -ltn | grep -q ':$p ' || break; sleep 1; done"; done; done
  for i in 0 1 2; do
    $NSSH ${IPS[$i]} "mkdir -p /mnt/nvme/wd; cp ~/bins/comp_log.yaml /mnt/nvme/wd/server_log.yaml; cd /mnt/nvme/wd; env $CENV $ENVX WARPDRIVE_SELF_ADDR=http://${IPS[$i]}:9710 WARPDRIVE_PEERS=$PEERS WARPDRIVE_RS_K=2 WARPDRIVE_RS_M=1 WARPDRIVE_PORT=9710 WARPDRIVE_ADMIN_ACCESS_KEY=$AK WARPDRIVE_ADMIN_SECRET_KEY=$SK setsid nohup ~/bins/$BIN > node.out 2>&1 < /dev/null & sleep 1; setsid nohup bash ~/bins/comp_sampler.sh $BIN /mnt/nvme/comp.csv > /dev/null 2>&1 < /dev/null &"; done
  ok=0; for k in $(seq 1 40); do curl -s -o /dev/null -w '%{http_code}' http://${IPS[0]}:9710/ | grep -q '^[234]' && { ok=1; break; }; sleep 1; done
  [ $ok = 1 ] || { log "${SIZE}_$ARM: node0 never answered"; continue; }
  log "${SIZE}_$ARM: start, $KEYS keys, $CONC clients, $DUR s per phase"
  S3_BENCH_KEYS=$KEYS S3_BENCH_TRACE=$D/trace.csv ~/bins/s3b-comp http://${IPS[0]}:9710 "" bench $CONC $DUR $SIZE > $D/bench.csv 2> $D/bench.err
  python3 - $D <<'PY'
import sys, collections
D=sys.argv[1]; per=collections.defaultdict(list); err=collections.Counter(); t0=None
for line in open(f"{D}/trace.csv"):
    p=line.split(",")
    if len(p)<4 or not p[1].isdigit(): continue
    s=int(p[1])//1_000_000; t0=s if t0 is None else min(t0,s); per[(p[0],s)].append(int(p[2]))
    if p[3].strip() not in ("1","true","ok"): err[(p[0],s)]+=1
with open(f"{D}/per_second.csv","w") as f:
    f.write("phase,second,ops,errors,p50_us,p99_us\n")
    for (ph,s),v in sorted(per.items(), key=lambda x:(x[0][0]!="PUT", x[0][1])):
        v.sort(); f.write(f"{ph},{s-t0},{len(v)},{err[(ph,s)]},{v[len(v)//2]},{v[min(len(v)-1,int(len(v)*0.99))]}\n")
PY
  rm -f $D/trace.csv
  for i in 0 1 2; do
    $NSSH ${IPS[$i]} "find /mnt/nvme/wd/storage -name '*.seg' | wc -l" > $D/node$i.segs
    curl -s -X POST http://${IPS[$i]}:9710/cluster/_internal/reclaim/report > $D/node$i.final_report.json
    $NSSH ${IPS[$i]} "pkill -f '[s]ampler.sh'; pkill -x $BIN; grep -c -i panicked /mnt/nvme/wd/node.out; grep -h 'compactor\|reclaim' /mnt/nvme/wd/logs/application.log | tail -400" > $D/node$i.log
    $SCP ${IPS[$i]}:/mnt/nvme/comp.csv $D/node$i.csv
  done
  log "${SIZE}_$ARM done: $(tr '\n' ' ' < $D/bench.csv)"
done; done
log "ALL DONE"
