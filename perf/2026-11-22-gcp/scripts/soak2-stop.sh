#!/usr/bin/env bash
# Runs on wlc: stops soak 2 early (its question is answered) and keeps what it measured.
R=~/soak2; log() { echo "$(date -u +%T) $*" >> $R/progress.log; }
IPS=($(cat ~/ips.txt)); NSSH="ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -n"
SCP="scp -q -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
pkill -f "^bash /home/cj/soak2.sh$"; pkill -x s3b-comp; pkill -f "[c]anary.py"; sleep 2
log "STOPPED EARLY at user request: gc/ row fix shown working; memory still creeps from the blob index leak (fixed in 766060b, soak 3)"
for i in 0 1 2; do
  curl -s -m 60 -X POST http://${IPS[$i]}:9710/cluster/_internal/reclaim/report > $R/node$i.audit.json
  $NSSH ${IPS[$i]} "pkill -f '[s]ampler.sh'; grep -c -i panicked /mnt/nvme/wd/node.out; grep -h 'compactor\|reclaim\|gc:' /mnt/nvme/wd/logs/application.log | tail -600; ls -la /mnt/nvme/wd/*.log" > $R/node$i.log
  $SCP ${IPS[$i]}:/mnt/nvme/soak.csv $R/node$i.csv
done
python3 - $R <<'PY'
import sys, collections, os
R=sys.argv[1]; per=collections.defaultdict(list); err=collections.Counter()
if os.path.exists(f"{R}/trace.csv"):
    for line in open(f"{R}/trace.csv"):
        p=line.split(",")
        if len(p)<4 or not p[1].isdigit(): continue
        s=int(p[1])//1_000_000; per[(p[0],s)].append(int(p[2]))
        if p[3].strip()!="true": err[(p[0],s)]+=1
    with open(f"{R}/per_second.csv","w") as f:
        f.write("phase,unix_s,ops,errors,p50_us,p99_us\n")
        for (ph,s),v in sorted(per.items(), key=lambda x:(x[0][1],x[0][0])):
            v.sort(); f.write(f"{ph},{s},{len(v)},{err[(ph,s)]},{v[len(v)//2]},{v[min(len(v)-1,int(len(v)*0.99))]}\n")
    os.remove(f"{R}/trace.csv")
PY
log "partial data collected"
