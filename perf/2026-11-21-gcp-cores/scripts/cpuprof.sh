#!/usr/bin/env bash
# Runs on wlc after cores.sh. Where does the CPU go per request? #226, RS(2,1), node0 pinned to 2 cores, 128 clients,
# 60 s PUT then 60 s GET. In each phase, on node0 (coordinator) and node1 (shard holder):
#   +20 s  perf record -g (dwarf) 15 s          -> flat profile, inclusive profile, flame graph
#   +38 s  perf stat + pidstat 10 s (node0)     -> cycles, IPC, context switches, user/sys per request
#   +50 s  strace -c 5 s (node0)                -> syscalls per request (strace slows the server; counts only)
until grep -q "ALL DONE" ~/cores/progress.log 2>/dev/null; do sleep 30; done
sleep 90
IPS=($(cat ~/ips.txt)); P=~/prof; rm -rf $P; mkdir -p $P
NSSH="ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -n"
SCP="scp -q -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
AK=AKIAIOSFODNN7EXAMPLE; SK=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY
log() { echo "$(date -u +%T) $*" >> $P/progress.log; }
log "start"
[ -d ~/FlameGraph ] || git clone -q --depth 1 https://github.com/brendangregg/FlameGraph ~/FlameGraph
for i in 0 1; do $NSSH ${IPS[$i]} 'sudo DEBIAN_FRONTEND=noninteractive apt-get -qq update >/dev/null; sudo DEBIAN_FRONTEND=noninteractive apt-get -qq install -y linux-tools-$(uname -r) linux-tools-gcp sysstat strace >/dev/null 2>&1; sudo perf --version' & done; wait
for i in 0 1; do $SCP ~/FlameGraph/stackcollapse-perf.pl ${IPS[$i]}:~/bins/; done
log "tools installed: $($NSSH ${IPS[0]} 'sudo perf --version')"
PEERS=$(printf "http://%s:9710," "${IPS[@]}"); PEERS=${PEERS%,}
for i in 0 1 2; do $NSSH ${IPS[$i]} "pkill -x wd-par; pkill -f '[s]ampler.sh'; sleep 1; rm -rf /mnt/nvme/wd /mnt/nvme/mem.csv /mnt/nvme/prof; mkdir -p /mnt/nvme/prof"; done
for i in 0 1 2; do TS=""; [ $i = 0 ] && TS="taskset -c 0-1"
  $NSSH ${IPS[$i]} "mkdir -p /mnt/nvme/wd; cp ~/bins/server_log.yaml /mnt/nvme/wd/; cd /mnt/nvme/wd; env WARPDRIVE_SELF_ADDR=http://${IPS[$i]}:9710 WARPDRIVE_PEERS=$PEERS WARPDRIVE_RS_K=2 WARPDRIVE_RS_M=1 WARPDRIVE_PORT=9710 WARPDRIVE_ADMIN_ACCESS_KEY=$AK WARPDRIVE_ADMIN_SECRET_KEY=$SK setsid nohup $TS ~/bins/wd-par > node.out 2>&1 < /dev/null &"; done
for k in $(seq 1 30); do curl -s -o /dev/null -w '%{http_code}' http://${IPS[0]}:9710/ | grep -q '^[234]' && break; sleep 1; done
python3 -c "
import boto3; from botocore.config import Config
boto3.client('s3', endpoint_url='http://${IPS[0]}:9710', aws_access_key_id='$AK', aws_secret_access_key='$SK', region_name='us-east-1', config=Config(s3={'addressing_style':'path'})).create_bucket(Bucket='bench')"
T0=$(date +%s); echo $T0 > $P/t0.txt
S3_BENCH_TRACE=$P/trace.csv ~/bins/s3_bench http://${IPS[0]}:9710 "" bench 128 60 4096 > $P/bench.csv 2> $P/bench.err &
BP=$!
capture() {  # $1 = phase, $2 = phase start offset
  ph=$1; off=$2
  sleep $(( T0 + off + 20 - $(date +%s) ))
  for i in 0 1; do $NSSH ${IPS[$i]} "cd /mnt/nvme/prof; sudo perf record -F 99 --call-graph dwarf,16384 -o $ph-node$i.data -p \$(pgrep -x wd-par) -- sleep 15 >/dev/null 2>&1" & done
  echo "$ph perf $(( $(date +%s) )) $(( $(date +%s) + 15 ))" >> $P/windows.txt; wait
  sleep $(( T0 + off + 38 - $(date +%s) ))
  echo "$ph stat $(date +%s) $(( $(date +%s) + 10 ))" >> $P/windows.txt
  $NSSH ${IPS[0]} "cd /mnt/nvme/prof; pid=\$(pgrep -x wd-par); sudo perf stat -e task-clock,cycles,instructions,context-switches,cpu-migrations,page-faults,cache-misses -p \$pid -- sleep 10 2> $ph-perfstat.txt & pidstat -u -w -h -p \$pid 10 1 > $ph-pidstat.txt; wait" 
  sleep $(( T0 + off + 50 - $(date +%s) ))
  echo "$ph strace $(date +%s) $(( $(date +%s) + 5 ))" >> $P/windows.txt
  $NSSH ${IPS[0]} "cd /mnt/nvme/prof; sudo timeout -s INT 5 strace -c -f -p \$(pgrep -x wd-par) -o $ph-strace.txt >/dev/null 2>&1; true"
  log "$ph captured"
}
capture put 0
capture get 60
wait $BP
log "bench done: $(tr '\n' ' ' < $P/bench.csv)"
for i in 0 1; do $NSSH ${IPS[$i]} "pkill -x wd-par; cd /mnt/nvme/prof; for ph in put get; do
    sudo perf report -i \$ph-node$i.data --no-children --sort symbol --stdio -g none --percent-limit 0.3 2>/dev/null | grep -v '^#' | grep -v '^\$' > \$ph-node$i-flat.txt
    sudo perf report -i \$ph-node$i.data --no-children --sort dso --stdio -g none 2>/dev/null | grep -v '^#' | grep -v '^\$' > \$ph-node$i-dso.txt
    sudo perf report -i \$ph-node$i.data --children --sort symbol --stdio -g none --percent-limit 3 2>/dev/null | grep -v '^#' | grep -v '^\$' > \$ph-node$i-inclusive.txt
    sudo perf script -i \$ph-node$i.data 2>/dev/null | perl ~/bins/stackcollapse-perf.pl > \$ph-node$i.folded
  done; sudo chown \$USER *; ls"; $SCP "${IPS[$i]}:/mnt/nvme/prof/*.txt" "${IPS[$i]}:/mnt/nvme/prof/*.folded" $P/; done
for f in $P/*.folded; do b=$(basename $f .folded); perl ~/FlameGraph/flamegraph.pl --title "#226 4 KB ${b/-/ }, RS(2,1), node0 on 2 cores, 128 clients" --width 1600 $f > $P/$b.svg; done
# Ops in each window, from the per-request trace (start time is epoch microseconds).
python3 - $P <<'PY'
import sys
P=sys.argv[1]
w=[l.split() for l in open(f"{P}/windows.txt")]
starts={"put":[], "get":[]}
for l in open(f"{P}/trace.csv"):
    ph, st = l.split(",", 2)[:2]
    starts.setdefault(ph, []).append(int(st)//1_000_000)
with open(f"{P}/window_ops.csv","w") as f:
    f.write("phase,kind,start,end,ops,ops_per_s\n")
    for ph, kind, a, b in w:
        n=sum(1 for s in starts.get(ph,[]) if int(a) <= s < int(b))
        f.write(f"{ph},{kind},{a},{b},{n},{n/(int(b)-int(a)):.1f}\n")
PY
rm -f $P/trace.csv
log "PROF DONE"
