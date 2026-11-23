#!/usr/bin/env bash
# Runs on wlc, detached: the soak #237 waits on. RocksDB with #238 (wd-combo, ce20fc3, 1 GB cache), RS(3,2) on the 5
# n2-standard-2 nodes. s3_bench overwrites 100,000 keys of 4 KB at 64 clients for 15 min, then reads them for 15 min.
# canary.py runs the whole 30 min (unique bytes per object, every GET and LIST checked, deletes, multipart uploads and
# aborts, a full read-back at the end). Every node sampled every 5 s. Then 240 s for every gc/ row to come due, and
# the audit sweep on every node. Results in ~/soak-final.
R=~/soak-final; rm -rf $R; mkdir -p $R; log() { echo "$(date -u +%T) $*" >> $R/progress.log; }
IPS=($(cat ~/ips.txt)); NSSH="ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -n"
SCP="scp -q -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
AK=AKIAIOSFODNN7EXAMPLE; SK=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY; PEERS=$(printf "http://%s:9710," "${IPS[@]}"); PEERS=${PEERS%,}
CENV="WARPDRIVE_META_ENGINE=rocksdb WARPDRIVE_ROCKSDB_CACHE_MB=1024 WARPDRIVE_SEGMENT_MB=64 WARPDRIVE_RECLAIM_GRACE_SECS=120 WARPDRIVE_RECLAIM_CONFIRM_SECS=60 WARPDRIVE_COMPACT_SEAL_SECS=120"
log "bin: wd-combo $(md5sum ~/bins/wd-combo | cut -c1-8) (ce20fc3), RS(3,2) on ${#IPS[@]} nodes: ${IPS[*]}"
for ip in "${IPS[@]}"; do $NSSH $ip "pkill -x wd-combo; pkill -x wd-final3; pkill -x wd-rocks; sleep 2; mountpoint -q /mnt/nvme || exit 1; rm -rf /mnt/nvme/wd /mnt/nvme/soak.csv; mkdir -p /mnt/nvme/wd; cp ~/bins/comp_log.yaml /mnt/nvme/wd/server_log.yaml" || { log "ABORT node $ip not ready"; exit 1; }; done
for ip in "${IPS[@]}"; do $NSSH $ip "for p in 9710 10710 11710; do for k in \$(seq 1 30); do ss -ltn | grep -q \":\$p \" || break; sleep 1; done; done"; done
for ip in "${IPS[@]}"; do $NSSH $ip "cd /mnt/nvme/wd; ulimit -n 1048576; env $CENV WARPDRIVE_SELF_ADDR=http://$ip:9710 WARPDRIVE_PEERS=$PEERS WARPDRIVE_RS_K=3 WARPDRIVE_RS_M=2 WARPDRIVE_PORT=9710 WARPDRIVE_ADMIN_ACCESS_KEY=$AK WARPDRIVE_ADMIN_SECRET_KEY=$SK setsid nohup ~/bins/wd-combo > node.out 2>&1 < /dev/null &"; done
for k in $(seq 1 60); do ok=0; for ip in "${IPS[@]}"; do curl -s -m 2 -o /dev/null -w '%{http_code}' http://$ip:9710/ | grep -q '^[234]' && ok=$((ok+1)); done; [ $ok = ${#IPS[@]} ] && break; sleep 1; done
[ $ok = ${#IPS[@]} ] || { log "ABORT cluster not ready"; exit 1; }
# Samplers: started, and later stopped, by PID (never pkill -f, which can match its own shell).
for ip in "${IPS[@]}"; do $NSSH $ip "setsid nohup bash ~/bins/soak_sampler.sh wd-combo /mnt/nvme/soak.csv > /dev/null 2>&1 < /dev/null & echo \$! > /mnt/nvme/sampler.pid"; done
( while sleep 5; do echo "$(date +%s),$(top -bn1 | awk '/^%Cpu/{print 100-$8}')" >> $R/client-cpu.csv; done ) & C=$!
log "start: s3_bench 4 KB overwrites of 100,000 keys at 64 clients 15 min + reads 15 min; canary 30 min"
python3 ~/canary.py http://${IPS[0]}:9710 1800 $R/canary > $R/canary.out 2>&1 &
CP=$!
S3_BENCH_KEYS=100000 S3_BENCH_TRACE=$R/trace.csv ~/bins/s3b-combo http://${IPS[0]}:9710 "" bench 64 900 4096 > $R/bench.csv 2> $R/bench.err
log "load done: $(tr '\n' ' ' < $R/bench.csv)"
wait $CP
log "canary: $(tail -1 $R/canary.out)"
log "waiting 240 s for every gc/ row to come due"; sleep 240
for i in "${!IPS[@]}"; do curl -s -m 120 -X POST http://${IPS[$i]}:9710/cluster/_internal/reclaim/report > $R/node$i.audit.json; done
log "audit: $(for i in "${!IPS[@]}"; do python3 -c "import json,sys; a=json.load(open('$R/node$i.audit.json')); print(a['reclaimable_superseded']+a['reclaimable_confirmed'], end=' ')"; done)"
kill $C 2>/dev/null
python3 - $R <<'PY'
import sys, collections
R=sys.argv[1]; per=collections.defaultdict(list); err=collections.Counter()
for line in open(f"{R}/trace.csv"):
    p=line.split(",")
    if len(p)<4 or not p[1].isdigit(): continue
    s=int(p[1])//1_000_000; per[(p[0],s)].append(int(p[2]))
    if p[3].strip()!="true": err[(p[0],s)]+=1
with open(f"{R}/per_second.csv","w") as f:
    f.write("phase,unix_s,ops,errors,p50_us,p99_us\n")
    for (ph,s),v in sorted(per.items(), key=lambda x:(x[0][1],x[0][0])):
        v.sort(); f.write(f"{ph},{s},{len(v)},{err[(ph,s)]},{v[len(v)//2]},{v[min(len(v)-1,int(len(v)*0.99))]}\n")
PY
rm -f $R/trace.csv
for i in "${!IPS[@]}"; do
  $NSSH ${IPS[$i]} "kill \$(cat /mnt/nvme/sampler.pid) 2>/dev/null; grep -c -i panicked /mnt/nvme/wd/node.out; grep -h 'compactor\|reclaim\|gc:' /mnt/nvme/wd/logs/application.log | tail -300; ls -la /mnt/nvme/wd/; du -sh /mnt/nvme/wd/*.rocks /mnt/nvme/wd/storage" > $R/node$i.log
  $SCP ${IPS[$i]}:/mnt/nvme/soak.csv $R/node$i.csv
done
log "ALL DONE"
