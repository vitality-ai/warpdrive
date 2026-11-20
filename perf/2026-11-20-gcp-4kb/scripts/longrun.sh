#!/usr/bin/env bash
# Runs on the client. For each config: fresh 5-node cluster at WARN, samplers on, s3_bench PUT then GET, collect.
IPS=(10.128.0.x ...)  # filled in per cluster by finish.sh
PEERS=$(printf "http://%s:9710," "${IPS[@]}"); PEERS=${PEERS%,}
NSSH="ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -n"
DUR=${DUR:-600}; CONC=${CONC:-32}; SIZE=${SIZE:-4096}
CONFIGS=${CONFIGS:-"217:wd-217: 217i8:wd-217:WARPDRIVE_INLINE_MAX_BYTES=8192 par:wd-par: par_i4:wd-par:WARPDRIVE_INLINE_MAX_BYTES=4096 par_i8:wd-par:WARPDRIVE_INLINE_MAX_BYTES=8192"}
syncs() { t=0; for ip in "${IPS[@]}"; do n=$(curl -s http://$ip:9710/cluster/_internal/timing | grep -o 'sync_count=[0-9]*' | cut -d= -f2); t=$((t+${n:-0})); done; echo $t; }
mkdir -p ~/res; echo "start $(date -u +%T) DUR=$DUR CONC=$CONC SIZE=$SIZE" >> ~/res/progress.log
for c in $CONFIGS; do
  NAME=${c%%:*}; rest=${c#*:}; BIN=${rest%%:*}; ENV=${rest#*:}
  D=~/res/$NAME; rm -rf $D; mkdir -p $D
  for i in 0 1 2 3 4; do { $NSSH ${IPS[$i]} "pkill -x wd-par; pkill -x wd-217; pkill -f '[s]ampler.sh'; sleep 1; rm -rf /mnt/nvme/wd /mnt/nvme/mem.csv"; $NSSH ${IPS[$i]} "mkdir -p /mnt/nvme/wd; cp ~/bins/server_log.yaml /mnt/nvme/wd/; cd /mnt/nvme/wd; env $ENV WARPDRIVE_PORT=9710 WARPDRIVE_SELF_ADDR=http://${IPS[$i]}:9710 WARPDRIVE_PEERS=$PEERS WARPDRIVE_RS_K=3 WARPDRIVE_RS_M=2 WARPDRIVE_ADMIN_ACCESS_KEY=AKIAIOSFODNN7EXAMPLE WARPDRIVE_ADMIN_SECRET_KEY=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY setsid nohup ~/bins/$BIN > node.out 2>&1 < /dev/null & sleep 1; setsid nohup bash ~/bins/sampler.sh $BIN /mnt/nvme/mem.csv > /dev/null 2>&1 < /dev/null &"; } & done; wait; sleep 3
  python3 -c "
import boto3; from botocore.config import Config
boto3.client('s3', endpoint_url='http://${IPS[0]}:9710', aws_access_key_id='AKIAIOSFODNN7EXAMPLE', aws_secret_access_key='wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY', region_name='us-east-1', config=Config(s3={'addressing_style':'path'})).create_bucket(Bucket='bench')"
  s0=$(syncs); T0=$(date +%s)
  S3_BENCH_TRACE=$D/trace.csv ~/bins/s3_bench http://${IPS[0]}:9710 "" bench $CONC $DUR $SIZE > $D/bench.csv 2> $D/bench.err
  s1=$(syncs); echo "$((s1-s0))" > $D/syncs.txt; echo "$T0" > $D/t0.txt
  for i in 0 1 2 3 4; do $NSSH ${IPS[$i]} "pkill -f '[s]ampler.sh'; pkill -x wd-par; pkill -x wd-217; grep -c -i 'error\|panicked' /mnt/nvme/wd/node.out" > $D/node$i.errors 2>&1; scp -q -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR ${IPS[$i]}:/mnt/nvme/mem.csv $D/mem$i.csv; done
  # Per-second throughput and latency percentiles from the per-request trace, then drop the raw trace.
  python3 - "$D" <<'PY'
import sys, collections
D=sys.argv[1]
b=collections.defaultdict(list)
t0=None
for l in open(f"{D}/trace.csv"):
    ph, start, lat, ok, _ = l.rstrip("\n").split(",", 4)
    s=int(start)//1_000_000
    if t0 is None or s<t0: t0=s
    b[(ph,s)].append((int(lat), ok=="true"))
with open(f"{D}/persec.csv","w") as f:
    f.write("phase,sec,ops,errors,p50_us,p99_us\n")
    for (ph,s),v in sorted(b.items(), key=lambda x:(x[0][0],x[0][1])):
        lats=sorted(x[0] for x in v); n=len(lats)
        f.write(f"{ph},{s-t0},{n},{sum(1 for x in v if not x[1])},{lats[n//2]},{lats[min(n-1,int(n*0.99))]}\n")
PY
  rm -f $D/trace.csv
  echo "$(date -u +%T) $NAME done: $(tr '\n' ' ' < $D/bench.csv) syncs=$(cat $D/syncs.txt)" >> ~/res/progress.log
done
echo "ALL DONE $(date -u +%T)" >> ~/res/progress.log
