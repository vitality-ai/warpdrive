#!/usr/bin/env bash
# Allocation microbenchmark: baseline compaction settings, XFS allocation varied.
# usage: run-alloc.sh <trial>. Results: ~/results/alloc.jsonl, extents in ~/results/alloc/.
set -u
TRIAL=$1; BIN=~/segbench/target/release/segbench
COMMON="--live-gb 6 --churn-x 3 --warm-x 1 --writers 8 --readers 4 --seed $TRIAL"
mkdir -p ~/results/alloc ~/results/tl
XDEV=$(df /mnt/xfs | tail -1 | awk '{print $1}')
grep -v '^#' ~/bench/alloc.txt | grep . | while IFS='|' read -r NAME FS ARGS; do
  sudo umount /mnt/xfs && sudo mount -o noatime "$XDEV" /mnt/xfs && sudo chown "$USER" /mnt/xfs
  PARENT=/mnt/$FS/$NAME; rm -rf "$PARENT"; mkdir -p "$PARENT"
  case $NAME in
    alloc-extsize) xfs_io -c "extsize 256m" "$PARENT" ;;   # subdirectories and files inherit it
    alloc-allocsize) sudo umount /mnt/xfs && sudo mount -o noatime,allocsize=64m "$XDEV" /mnt/xfs && sudo chown "$USER" /mnt/xfs; mkdir -p "$PARENT" ;;
  esac
  sync; echo 3 | sudo tee /proc/sys/vm/drop_caches > /dev/null
  # Extents per segment file, sampled every 30 s while the run goes.
  ( while sleep 30; do f=$(ls "$PARENT"/sb/*.seg 2>/dev/null | head -50); [ -z "$f" ] && continue
      filefrag $f 2>/dev/null | awk -v t=$(date +%s) '{e+=$(NF-2); n++} END {if (n) print t, n, e/n}' ; done ) > ~/results/alloc/$NAME-t$TRIAL.frag &
  FRAG=$!
  OUT=$($BIN --dir "$PARENT/sb" $COMMON $ARGS --timeline ~/results/tl/$NAME-t$TRIAL.csv 2> ~/results/tl/$NAME-t$TRIAL.err)
  kill $FRAG 2>/dev/null
  EXT=$(awk '{s+=$3; n++} END {if (n) printf "%.1f", s/n; else print 0}' ~/results/alloc/$NAME-t$TRIAL.frag)
  echo "{\"name\":\"$NAME\",\"fs\":\"$FS\",\"trial\":$TRIAL,\"extents_per_file\":$EXT,\"r\":$OUT}" >> ~/results/alloc.jsonl
  echo "$(date +%T) $NAME done"
done
sudo umount /mnt/xfs && sudo mount -o noatime "$XDEV" /mnt/xfs && sudo chown "$USER" /mnt/xfs
echo "ALLOC DONE $(date +%T)"
