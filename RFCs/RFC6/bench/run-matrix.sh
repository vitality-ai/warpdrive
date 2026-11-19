#!/usr/bin/env bash
# Runs on each VM: every configuration once, in a random order seeded by the
# trial number, so each VM is one trial. Results: ~/results/matrix.jsonl.
# usage: run-matrix.sh <trial> [configs-file] [results-file-name]
# MON=1 also records sar (CPU) and iostat (device) per run in ~/results/mon/.
set -u
TRIAL=$1; CONFIGS=${2:-~/bench/configs.txt}; RFILE=${3:-matrix.jsonl}
BIN=~/segbench/target/release/segbench
mkdir -p ~/results/tl
COMMON="--live-gb 6 --churn-x 3 --warm-x 1 --writers 8 --readers 4 --seed $TRIAL ${EXTRA:-}"
grep -v '^#' "$CONFIGS" | grep . | shuf --random-source=<(yes $TRIAL) > ~/results/order-$TRIAL.txt
while IFS='|' read -r NAME FS ARGS; do
  sync; echo 3 | sudo tee /proc/sys/vm/drop_caches > /dev/null
  if [ "${MON:-0}" = 1 ]; then
    mkdir -p ~/results/mon
    DEVN=$(df /mnt/$FS | tail -1 | awk '{print $1}' | sed 's|/dev/||; s|p[0-9]*$||')
    setsid nohup sar -u 1 > ~/results/mon/$NAME-t$TRIAL.sar 2>&1 < /dev/null & SARP=$!
    setsid nohup iostat -dxm 1 $DEVN > ~/results/mon/$NAME-t$TRIAL.iostat 2>&1 < /dev/null & IOP=$!
  fi
  OUT=$($BIN --dir /mnt/$FS/sb $COMMON $ARGS --timeline ~/results/tl/$NAME-t$TRIAL.csv 2> ~/results/tl/$NAME-t$TRIAL.err)
  [ "${MON:-0}" = 1 ] && kill $SARP $IOP 2>/dev/null
  echo "{\"name\":\"$NAME\",\"fs\":\"$FS\",\"trial\":$TRIAL,\"r\":$OUT}" >> ~/results/$RFILE
  echo "$(date +%T) $NAME done"
done < ~/results/order-$TRIAL.txt
echo "ALL DONE $(date +%T)"
