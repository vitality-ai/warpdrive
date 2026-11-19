#!/usr/bin/env bash
# Runs on each VM: every configuration once, in a random order seeded by the
# trial number, so each VM is one trial. Results: ~/results/matrix.jsonl.
# usage: run-matrix.sh <trial> [configs-file] [results-file-name]
set -u
TRIAL=$1; CONFIGS=${2:-~/bench/configs.txt}; RFILE=${3:-matrix.jsonl}
BIN=~/segbench/target/release/segbench
mkdir -p ~/results/tl
COMMON="--live-gb 6 --churn-x 3 --warm-x 1 --writers 8 --readers 4 --seed $TRIAL"
grep -v '^#' "$CONFIGS" | grep . | shuf --random-source=<(yes $TRIAL) > ~/results/order-$TRIAL.txt
while IFS='|' read -r NAME FS ARGS; do
  sync; echo 3 | sudo tee /proc/sys/vm/drop_caches > /dev/null
  OUT=$($BIN --dir /mnt/$FS/sb $COMMON $ARGS --timeline ~/results/tl/$NAME-t$TRIAL.csv 2> ~/results/tl/$NAME-t$TRIAL.err)
  echo "{\"name\":\"$NAME\",\"fs\":\"$FS\",\"trial\":$TRIAL,\"r\":$OUT}" >> ~/results/$RFILE
  echo "$(date +%T) $NAME done"
done < ~/results/order-$TRIAL.txt
echo "ALL DONE $(date +%T)"
