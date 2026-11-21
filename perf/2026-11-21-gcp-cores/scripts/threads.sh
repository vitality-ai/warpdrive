#!/usr/bin/env bash
# Runs on wlc. Context switches per thread pool on node0 (and node1), per request. RS(2,1), node0 on 2 cores, pool arm, 128 clients.
IPS=($(cat ~/ips.txt)); NSSH="ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -n"
AK=AKIAIOSFODNN7EXAMPLE; SK=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY; PEERS=$(printf "http://%s:9710," "${IPS[@]}"); PEERS=${PEERS%,}
R=~/threads; rm -rf $R; mkdir -p $R
for i in 0 1 2; do $NSSH ${IPS[$i]} "pkill -x wd-par; pkill -f '[s]ampler.sh'; sleep 1; rm -rf /mnt/nvme/wd"; done
for i in 0 1 2; do TS=""; [ $i = 0 ] && TS="taskset -c 0-1"
  $NSSH ${IPS[$i]} "mkdir -p /mnt/nvme/wd; cp ~/bins/server_log.yaml /mnt/nvme/wd/; cd /mnt/nvme/wd; env WARPDRIVE_SELF_ADDR=http://${IPS[$i]}:9710 WARPDRIVE_PEERS=$PEERS WARPDRIVE_RS_K=2 WARPDRIVE_RS_M=1 WARPDRIVE_PORT=9710 WARPDRIVE_ADMIN_ACCESS_KEY=$AK WARPDRIVE_ADMIN_SECRET_KEY=$SK setsid nohup $TS ~/bins/wd-par > node.out 2>&1 < /dev/null &"; done
for k in $(seq 1 30); do curl -s -o /dev/null -w '%{http_code}' http://${IPS[0]}:9710/ | grep -q '^[234]' && break; sleep 1; done
python3 -c "
import boto3; from botocore.config import Config
boto3.client('s3', endpoint_url='http://${IPS[0]}:9710', aws_access_key_id='$AK', aws_secret_access_key='$SK', region_name='us-east-1', config=Config(s3={'addressing_style':'path'})).create_bucket(Bucket='bench')"
# per-thread snapshot: name, voluntary, nonvoluntary, utime+stime ticks
snap() { $NSSH ${IPS[$1]} 'p=$(pgrep -x wd-par); for t in /proc/$p/task/*; do n=$(cat $t/comm); v=$(awk "/^voluntary_ctxt/{print \$2}" $t/status); nv=$(awk "/^nonvoluntary_ctxt/{print \$2}" $t/status); c=$(awk "{print \$14+\$15}" $t/stat); echo "$(basename $t) $n|$v|$nv|$c"; done'; }
S3_BENCH_TRACE=$R/trace.csv ~/bins/s3_bench http://${IPS[0]}:9710 "" bench 128 60 4096 > $R/bench.csv 2> $R/bench.err &
T0=$(date +%s)
for ph in put get; do
  off=0; [ $ph = get ] && off=60
  sleep $(( T0 + off + 20 - $(date +%s) )); a=$(date +%s); for i in 0 1; do snap $i > $R/$ph-n$i-a.txt; done
  sleep $(( T0 + off + 50 - $(date +%s) )); b=$(date +%s); for i in 0 1; do snap $i > $R/$ph-n$i-b.txt; done
  echo "$ph $a $b" >> $R/windows.txt
done
wait
python3 - $R <<'PY'
import sys, collections
R=sys.argv[1]
w={l.split()[0]:(int(l.split()[1]),int(l.split()[2])) for l in open(f"{R}/windows.txt")}
ops=collections.Counter()
for l in open(f"{R}/trace.csv"):
    ph,st=l.split(",",2)[:2]; s=int(st)//1_000_000
    if ph in w and w[ph][0]<=s<w[ph][1]: ops[ph]+=1
def load(p):
    d={}
    for l in open(p):
        tid,rest=l.strip().split(" ",1); n,v,nv,c=rest.rsplit("|",3); d[tid]=(n,int(v),int(nv),int(c))
    return d
for ph in ("put","get"):
    for node in ("n0","n1"):
        a,b=load(f"{R}/{ph}-{node}-a.txt"),load(f"{R}/{ph}-{node}-b.txt")
        agg=collections.defaultdict(lambda:[0,0,0,0])
        for tid,(n,v,nv,c) in b.items():
            n0=n.rstrip("0123456789-") or n
            pv=a.get(tid,(n,0,0,0))
            g=agg[n0]; g[0]+=1; g[1]+=v-pv[1]; g[2]+=nv-pv[2]; g[3]+=c-pv[3]
        o=ops[ph]; tv=sum(x[1]+x[2] for x in agg.values())
        print(f"== {ph} {node}: {o} requests in window ({o/(w[ph][1]-w[ph][0]):.0f}/s), {tv/o:.1f} context switches per request")
        print(f"   {'thread pool':24} threads  vol/req  invol/req  cpu_us/req")
        for n,(k,v,nv,c) in sorted(agg.items(), key=lambda x:-(x[1][1]+x[1][2])):
            if v+nv==0 and c==0: continue
            print(f"   {n:24} {k:5}  {v/o:7.2f}  {nv/o:8.2f}  {c*10000/o:9.0f}")
PY
