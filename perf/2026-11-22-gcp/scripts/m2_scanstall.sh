#!/usr/bin/env bash
# Runs on wlc after m1_getprof.sh. Microbenchmark 2: does a reclaim sweep stall foreground PUTs?
# Locked scans (wd-comp, 44a4055) vs chunked scans (wd-cgc, 9985702). RS(2,1), 100,000 keys of 4 KB overwritten by
# 128 clients, grace 30 s; at +90 s node0 runs one full sweep (POST /cluster/_internal/reclaim/report), whose peer
# confirmations scan every metadata row on every node. Per-second PUT rate and p99 around it.
until grep -q "ALL DONE\|ABORT" ~/micro/getprof/progress.log 2>/dev/null; do sleep 30; done
R=~/micro/scanstall; rm -rf $R; mkdir -p $R; log() { echo "$(date -u +%T) $*" >> $R/progress.log; }
IPS=($(cat ~/ips.txt)); NSSH="ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -n"
AK=AKIAIOSFODNN7EXAMPLE; SK=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY; PEERS=$(printf "http://%s:9710," "${IPS[@]}"); PEERS=${PEERS%,}
log "bins: $(sha256sum ~/bins/wd-comp ~/bins/wd-cgc | cut -c1-16 | tr '\n' ' ')"
for ARM in comp cgc cgc comp; do
  BIN=wd-$ARM; D=$R/$ARM-$(date +%s); mkdir -p $D
  for i in 0 1 2; do $NSSH ${IPS[$i]} "pkill -x wd-comp; pkill -x wd-cgc; pkill -x wd-main; pkill -x wd-as; pkill -f '[s]ampler.sh'; sleep 1; rm -rf /mnt/nvme/wd"; done
  for i in 0 1 2; do for p in 9710 10710 11710; do $NSSH ${IPS[$i]} "for k in \$(seq 1 30); do ss -ltn | grep -q ':$p ' || break; sleep 1; done"; done; done
  for i in 0 1 2; do $NSSH ${IPS[$i]} "mkdir -p /mnt/nvme/wd; cp ~/bins/server_log.yaml /mnt/nvme/wd/; cd /mnt/nvme/wd; env WARPDRIVE_RECLAIM_GRACE_SECS=30 WARPDRIVE_RECLAIM_INTERVAL_SECS=999999 WARPDRIVE_SEGMENT_MB=64 WARPDRIVE_SELF_ADDR=http://${IPS[$i]}:9710 WARPDRIVE_PEERS=$PEERS WARPDRIVE_RS_K=2 WARPDRIVE_RS_M=1 WARPDRIVE_PORT=9710 WARPDRIVE_ADMIN_ACCESS_KEY=$AK WARPDRIVE_ADMIN_SECRET_KEY=$SK setsid nohup ~/bins/$BIN > node.out 2>&1 < /dev/null &"; done
  for k in $(seq 1 40); do curl -s -o /dev/null -w '%{http_code}' http://${IPS[0]}:9710/ | grep -q '^[234]' && break; sleep 1; done
  T0=$(date +%s)
  S3_BENCH_KEYS=100000 S3_BENCH_TRACE=$D/trace.csv ~/bins/s3b-comp http://${IPS[0]}:9710 "" bench 128 200 4096 > $D/bench.csv 2> $D/bench.err &
  BP=$!
  sleep $(( T0 + 90 - $(date +%s) ))
  S0=$(date +%s.%N); curl -s -X POST --max-time 900 http://${IPS[0]}:9710/cluster/_internal/reclaim/report > $D/sweep.json; S1=$(date +%s.%N)
  echo "$S0,$S1" > $D/sweep_window.csv
  wait $BP
  python3 - $D <<'PY'
import sys, collections
D=sys.argv[1]; per=collections.defaultdict(list)
for line in open(f"{D}/trace.csv"):
    p=line.split(",")
    if len(p)<4 or p[0]!="PUT" and p[0]!="put" or not p[1].isdigit(): continue
    per[int(p[1])//1_000_000].append(int(p[2]))
with open(f"{D}/put_per_second.csv","w") as f:
    f.write("unix_s,ops,p50_us,p99_us\n")
    for s,v in sorted(per.items()):
        v.sort(); f.write(f"{s},{len(v)},{v[len(v)//2]},{v[min(len(v)-1,int(len(v)*0.99))]}\n")
PY
  rm -f $D/trace.csv
  log "$ARM done: sweep $(python3 -c "a,b=open('$D/sweep_window.csv').read().split(',');print(f'{float(b)-float(a):.1f} s')") $(cut -c1-160 $D/sweep.json) | $(head -1 $D/bench.csv)"
done
log "ALL DONE"
