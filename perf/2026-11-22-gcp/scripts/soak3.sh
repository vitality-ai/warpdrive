#!/usr/bin/env bash
# Runs on wlc. Soak 3: the same hour on 766060b (gc/ passes also drop dead blobs from the address index),
# to compare with soak 1 (4c2aeeb) and soak 2 (1f8bb42, stopped at ~35 min): RS(2,1) on wln-0..2, gc/ rows +
# compactor on. Background: s3_bench overwriting 100,000 keys of 4 KB at 64 clients for 30 min, then reading for 30 min.
# Alongside, for the whole hour: canary.py (8 threads) keeps its own objects with unique bytes, checks every GET and LIST,
# deletes, runs multipart uploads and aborts, then reads every object back at the end.
R=~/soak3; rm -rf $R; mkdir -p $R; log() { echo "$(date -u +%T) $*" >> $R/progress.log; }
IPS=($(cat ~/ips.txt)); NSSH="ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -n"
SCP="scp -q -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
. $HOME/.cargo/env
rm -rf ~/src-final3; mkdir -p ~/src-final3; tar xzf ~/src-final3.tgz -C ~/src-final3 2>/dev/null
(cd ~/src-final3/server && CARGO_TARGET_DIR=$HOME/tgt-comp cargo build --release --bin warp_drive 2>&1 | tail -1) > $R/build.log
grep -q Finished $R/build.log || { log "ABORT build failed"; exit 1; }
cp ~/tgt-comp/release/warp_drive ~/bins/wd-final3
log "bin: $(cat ~/src-final3/COMMIT) $(sha256sum ~/bins/wd-final3 | cut -c1-16)"
for ip in "${IPS[@]}"; do $NSSH $ip "mountpoint -q /mnt/nvme" || { log "ABORT no nvme $ip"; exit 1; }; $SCP ~/bins/wd-final3 ~/soak_sampler.sh ~/comp_log.yaml $ip:~/bins/; done
AK=AKIAIOSFODNN7EXAMPLE; SK=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY; PEERS=$(printf "http://%s:9710," "${IPS[@]}"); PEERS=${PEERS%,}
CENV="WARPDRIVE_SEGMENT_MB=64 WARPDRIVE_RECLAIM_GRACE_SECS=120 WARPDRIVE_RECLAIM_CONFIRM_SECS=60 WARPDRIVE_COMPACT_SEAL_SECS=120"
for i in 0 1 2; do $NSSH ${IPS[$i]} "pkill -x wd-cgc; pkill -x wd-final; pkill -x wd-final2; pkill -x wd-final3; pkill -x wd-csweep; pkill -x wd-comp; pkill -x wd-compold; pkill -f '[s]ampler.sh'; sleep 1; rm -rf /mnt/nvme/wd /mnt/nvme/soak.csv"; done
for i in 0 1 2; do for p in 9710 10710 11710; do $NSSH ${IPS[$i]} "for k in \$(seq 1 30); do ss -ltn | grep -q ':$p ' || break; sleep 1; done"; done; done
for i in 0 1 2; do
  $NSSH ${IPS[$i]} "mkdir -p /mnt/nvme/wd; cp ~/bins/comp_log.yaml /mnt/nvme/wd/server_log.yaml; cd /mnt/nvme/wd; ulimit -n 1048576; env $CENV WARPDRIVE_SELF_ADDR=http://${IPS[$i]}:9710 WARPDRIVE_PEERS=$PEERS WARPDRIVE_RS_K=2 WARPDRIVE_RS_M=1 WARPDRIVE_PORT=9710 WARPDRIVE_ADMIN_ACCESS_KEY=$AK WARPDRIVE_ADMIN_SECRET_KEY=$SK setsid nohup ~/bins/wd-final3 > node.out 2>&1 < /dev/null & sleep 1; setsid nohup bash ~/bins/soak_sampler.sh wd-final3 /mnt/nvme/soak.csv > /dev/null 2>&1 < /dev/null &"; done
for k in $(seq 1 40); do curl -s -o /dev/null -w '%{http_code}' http://${IPS[0]}:9710/ | grep -q '^[234]' && break; sleep 1; done
log "start: s3_bench 4 KB overwrites at 64 clients 30 min + reads 30 min; canary 60 min"
python3 ~/canary.py http://${IPS[0]}:9710 3600 $R/canary > $R/canary.out 2>&1 &
CP=$!
S3_BENCH_KEYS=100000 S3_BENCH_TRACE=$R/trace.csv ~/bins/s3b-comp http://${IPS[0]}:9710 "" bench 64 1800 4096 > $R/bench.csv 2> $R/bench.err
log "load done: $(tr '\n' ' ' < $R/bench.csv)"
wait $CP
log "canary: $(cat $R/canary.out | tail -1)"
python3 - $R <<'PY'
import sys, collections
R=sys.argv[1]; per=collections.defaultdict(list); err=collections.Counter(); t0=None
for line in open(f"{R}/trace.csv"):
    p=line.split(",")
    if len(p)<4 or not p[1].isdigit(): continue
    s=int(p[1])//1_000_000; t0=s if t0 is None else min(t0,s); per[(p[0],s)].append(int(p[2]))
    if p[3].strip()!="true": err[(p[0],s)]+=1
with open(f"{R}/per_second.csv","w") as f:
    f.write("phase,unix_s,ops,errors,p50_us,p99_us\n")
    for (ph,s),v in sorted(per.items(), key=lambda x:(x[0][1],x[0][0])):
        v.sort(); f.write(f"{ph},{s},{len(v)},{err[(ph,s)]},{v[len(v)//2]},{v[min(len(v)-1,int(len(v)*0.99))]}\n")
PY
rm -f $R/trace.csv
for i in 0 1 2; do
  curl -s -X POST http://${IPS[$i]}:9710/cluster/_internal/reclaim/report > $R/node$i.audit.json
  $NSSH ${IPS[$i]} "pkill -f '[s]ampler.sh'; grep -c -i panicked /mnt/nvme/wd/node.out; grep -h 'compactor\|reclaim\|gc:' /mnt/nvme/wd/logs/application.log | tail -600; ls -la /mnt/nvme/wd/*.log" > $R/node$i.log
  $SCP ${IPS[$i]}:/mnt/nvme/soak.csv $R/node$i.csv
done
log "ALL DONE"
