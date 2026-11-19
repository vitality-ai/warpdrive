#!/usr/bin/env bash
# Runs on wd-client. Results in ~/results/<run>/.
set -u
NODES=(10.128.0.5 10.128.0.6 10.128.0.2 10.128.0.3 10.128.0.4)
EP=http://10.128.0.5:9710
SSH="ssh -o StrictHostKeyChecking=no -o BatchMode=yes"
DUR=${DUR:-10}
mkdir -p ~/results
SUMMARY=~/results/summary.csv
[ -f $SUMMARY ] || echo "run,variant,phase,payload_bytes,concurrency,ops,errors,throughput_ops_s,mean_us,p50_us,p90_us,p99_us,syncs_total,trial" > $SUMMARY

variant_bin() { case $1 in B0) echo wd-main;; *) echo wd-proto;; esac; }
variant_env() { case $1 in
  B0|B1) echo "WARPDRIVE_BENCH=1";;
  B2) echo "WARPDRIVE_INLINE_MAX_BYTES=65536";;
  B3) echo "WARPDRIVE_INLINE_MAX_BYTES=65536 WARPDRIVE_SKIP_LOCAL_CLEANUP=1";;
esac; }
syncs() { local t=0; for n in "${NODES[@]}"; do v=$($SSH $n '~/node.sh syncs'); t=$((t + ${v:-0})); done; echo $t; }

run() {  # run <variant> <size> <conc> <trial> [perf]
  local V=$1 SZ=$2 C=$3 TR=$4 PERF=${5:-}
  local RUN="${V}_s${SZ}_c${C}_t${TR}${PERF:+_perf}"
  local D=~/results/$RUN; mkdir -p $D
  # -n plus a remote redirect: the servers and monitors node.sh starts would
  # otherwise keep the session's output open and ssh would never return.
  for n in "${NODES[@]}"; do $SSH -n $n "~/node.sh start $(variant_bin $V) $RUN $(variant_env $V) > /dev/null 2>&1" & done; wait
  sleep 2
  local s0=$(syncs)
  setsid nohup sar -u -r -n DEV 1 > $D/client_sar.txt 2>&1 < /dev/null & local SARPID=$!
  local PERFPID=""
  if [ -n "$PERF" ]; then
    $SSH -n ${NODES[0]} "sudo perf record -F 199 -g -o /mnt/nvme/mon/$RUN/perf.data -p \$(pgrep -f '[b]ins/wd-' | head -1) -- sleep $((DUR-1)) >/dev/null 2>&1" &
    PERFPID=$!
  fi
  S3_BENCH_TRACE=$D/client_trace.csv ~/bins/s3_bench $EP /s3 bench $C $DUR $SZ > $D/bench.csv 2> $D/bench.err
  # Wait only for perf. A bare `wait` would also wait on the sar monitor.
  [ -n "$PERFPID" ] && wait $PERFPID
  kill $SARPID 2>/dev/null
  local s1=$(syncs)
  for n in "${NODES[@]}"; do $SSH -n $n "~/node.sh stop $RUN > /dev/null 2>&1" & done; wait
  if [ -n "$PERF" ]; then $SSH ${NODES[0]} "sudo perf script -i /mnt/nvme/mon/$RUN/perf.data 2>/dev/null | gzip > /mnt/nvme/mon/$RUN/perf.script.gz"; fi
  local i=0; for n in "${NODES[@]}"; do mkdir -p $D/node$i; scp -q -o StrictHostKeyChecking=no "$n:/mnt/nvme/mon/$RUN/*" $D/node$i/ 2>/dev/null; i=$((i+1)); done
  while IFS= read -r line; do
    phase=${line%%,*}; [ "$phase" = put ] || [ "$phase" = get ] || continue
    echo "$RUN,$V,$line,$((s1 - s0)),$TR" >> $SUMMARY
  done < $D/bench.csv
  echo "$(date +%T) $RUN: $(tr '\n' ' ' < $D/bench.csv) syncs=$((s1 - s0))"
}

case "${1:-all}" in
smoke) run B0 4096 16 0; run B3 4096 16 0 ;;
all)
  for TR in 1 2; do for SZ in 1024 4096 16384 65536 262144 1048576; do for V in B0 B1 B2 B3; do run $V $SZ 16 $TR; done; done; done
  for C in 1 4 16 64; do for V in B0 B3; do run $V 4096 $C 1; done; done
  for V in B0 B3; do run $V 4096 16 1 perf; done
  echo "ALL DONE $(date +%T)" ;;
esac
