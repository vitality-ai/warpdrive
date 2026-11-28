#!/usr/bin/env bash
# zs3.sh, on wlc, detached: the full blob compression sweep. 5 x n2-standard-2, RS(3,2), RocksDB.
# One source tree (~/src-z3.tgz) built three ways: default flags (wd-base), native (wd-nat: RUSTFLAGS
# target-cpu=native, CFLAGS -march=native, which includes AVX-512 here) and AVX2 without AVX-512 (wd-v3:
# target-cpu and -march x86-64-v3).
#   native:  text 64 KB and 1 MB, arms off and zstd, each build in BUILDS, NREPS runs, interleaved
#   sweep:   SWEEP_BIN, data x size x arm (off, zstd, auto), REPS runs, arm order rotated every run
#   clients: SWEEP_BIN, text 1 MB, arms off zstd auto, at CLIENTS client counts, one run each
# Per run: fresh cluster, 60 s PUT then 60 s GET from s3_bench on wlc to node 0. Every node: NIC/NVMe
# counters every 2 s (netmon.sh), CPU every 5 s (soak_sampler.sh). Auto's view: raw/<tag>-decisions.txt.
source ~/.cargo/env; R=${R:-~/zs3}; rm -rf $R; mkdir -p $R/raw; exec > $R/driver.log 2>&1; log() { echo "$(date -u +%T) $*" | tee -a $R/progress.log; }
REPS=${REPS:-3}; DUR=${DUR:-60}; DATAS=${DATAS:-"text random"}; SPECS=${SPECS:-"4096:64 65536:64 1048576:32 8388608:16"}
CLIENTS=${CLIENTS:-"8 16 32 64 128"}; STAGES=${STAGES:-"native sweep clients"}
BUILDS=${BUILDS:-"wd-base wd-nat wd-v3"}; NREPS=${NREPS:-2}; SWEEP_BIN=${SWEEP_BIN:-wd-base}
log "build"
rm -rf ~/src-z3; mkdir -p ~/src-z3; tar xzf ~/src-z3.tgz -C ~/src-z3
(cd ~/src-z3/server && CARGO_TARGET_DIR=~/tgt-redis cargo build --release -q --bin warp_drive --bin s3_bench) && cp ~/tgt-redis/release/warp_drive ~/bins/wd-base && cp ~/tgt-redis/release/s3_bench ~/bins/s3b-z3 || { log "BUILD FAILED base"; exit 1; }
(cd ~/src-z3/server && CARGO_TARGET_DIR=~/tgt-native RUSTFLAGS="-C target-cpu=native" CFLAGS="-O3 -march=native" cargo build --release -q --bin warp_drive) && cp ~/tgt-native/release/warp_drive ~/bins/wd-nat || { log "BUILD FAILED native"; exit 1; }
(cd ~/src-z3/server && CARGO_TARGET_DIR=~/tgt-v3 RUSTFLAGS="-C target-cpu=x86-64-v3" CFLAGS="-O3 -march=x86-64-v3" CXXFLAGS="-include cstdint -mpclmul" cargo build --release -q --bin warp_drive) && cp ~/tgt-v3/release/warp_drive ~/bins/wd-v3 || { log "BUILD FAILED v3"; exit 1; }
log "wd-base $(sha256sum ~/bins/wd-base | cut -c1-12) wd-nat $(sha256sum ~/bins/wd-nat | cut -c1-12) wd-v3 $(sha256sum ~/bins/wd-v3 | cut -c1-12) s3b-z3 $(sha256sum ~/bins/s3b-z3 | cut -c1-12) src $(cat ~/src-z3/COMMIT 2>/dev/null)"
IPS=($(cat ~/ips.txt)); NSSH="ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -n"
SCP="scp -q -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
AK=AKIAIOSFODNN7EXAMPLE; SK=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY; PEERS=$(printf "http://%s:9710," "${IPS[@]}"); PEERS=${PEERS%,}
BASE="WARPDRIVE_META_ENGINE=rocksdb WARPDRIVE_ROCKSDB_CACHE_MB=1024 WARPDRIVE_SEGMENT_MB=64 WARPDRIVE_RECLAIM_GRACE_SECS=120 WARPDRIVE_RECLAIM_CONFIRM_SECS=60 WARPDRIVE_COMPACT_SEAL_SECS=120"
KILL="for b in wd-244 wd-pr wd-prb wd-fe wd-z wd-redis ba wd-base wd-nat wd-v3; do pkill -x \$b; done"
for ip in "${IPS[@]}"; do $NSSH $ip "$KILL; sleep 3"; done
for ip in "${IPS[@]}"; do $SCP ~/bins/wd-base ~/bins/wd-nat ~/bins/wd-v3 ~/netmon.sh $ip:bins/ || { log "ABORT scp $ip"; exit 1; }; done
ready() { for k in $(seq 1 300); do ok=0; for ip in "${IPS[@]}"; do curl -s -m 2 -o /dev/null -w '%{http_code}' http://$ip:9710/ | grep -q '^[234]' && ok=$((ok+1)); done; [ $ok = 5 ] && return 0; sleep 1; done; return 1; }
fresh() {  # fresh <binary> <extra env>
  for ip in "${IPS[@]}"; do $NSSH $ip "$KILL; for k in \$(seq 1 60); do pgrep -x wd-base >/dev/null || pgrep -x wd-nat >/dev/null || pgrep -x wd-v3 >/dev/null || pgrep -x wd-z >/dev/null || break; sleep 1; done; for p in 9710 10710 11710; do for k in \$(seq 1 30); do ss -ltn | grep -q \":\$p \" || break; sleep 1; done; done; rm -rf /mnt/nvme/wd; mkdir -p /mnt/nvme/wd; cp ~/bins/comp_log.yaml /mnt/nvme/wd/server_log.yaml" & done; wait
  for ip in "${IPS[@]}"; do $NSSH $ip "cd /mnt/nvme/wd; ulimit -n 1048576; env $BASE $2 WARPDRIVE_SELF_ADDR=http://$ip:9710 WARPDRIVE_PEERS=$PEERS WARPDRIVE_RS_K=3 WARPDRIVE_RS_M=2 WARPDRIVE_PORT=9710 WARPDRIVE_ADMIN_ACCESS_KEY=$AK WARPDRIVE_ADMIN_SECRET_KEY=$SK setsid nohup ~/bins/$1 > node.out 2>&1 < /dev/null &"; done
  ready || return 1; sleep 5; }
echo "stage,build,data,arm,size,clients,rep,phase,ops,errors,ops_per_s,mean_us,p50_us,p90_us,p99_us" > $R/summary.csv
run() {  # run <stage> <binary> <data> <arm> <size> <clients> <rep>
  local stage=$1 bin=$2 data=$3 arm=$4 size=$5 clients=$6 rep=$7 env=""
  [ $arm = zstd ] && env="WARPDRIVE_BLOB_COMPRESSION=zstd"; [ $arm = auto ] && env="WARPDRIVE_BLOB_COMPRESSION=auto"
  local tag=$stage-$bin-$data-$arm-$size-c$clients-r$rep
  fresh $bin "$env" || { log "ABORT $tag: cluster not ready"; exit 1; }
  for ip in "${IPS[@]}"; do $NSSH $ip "rm -f /mnt/nvme/net.txt /mnt/nvme/s.csv; setsid nohup bash ~/bins/netmon.sh /mnt/nvme/net.txt > /dev/null 2>&1 < /dev/null & echo \$! > /mnt/nvme/netmon.pid; setsid nohup bash ~/bins/soak_sampler.sh $bin /mnt/nvme/s.csv > /dev/null 2>&1 < /dev/null & echo \$! > /mnt/nvme/sampler.pid"; done
  sleep 3; local t0=$(date +%s)
  S3B_DATA=$data ~/bins/s3b-z3 http://${IPS[0]}:9710 "" bench $clients $DUR $size > $R/raw/$tag.csv 2> $R/raw/$tag.err
  local t1=$(date +%s); curl -s http://${IPS[0]}:9710/cluster/_internal/timing | grep compression= > $R/raw/$tag-decisions.txt; sleep 3
  local i=0; for ip in "${IPS[@]}"; do $NSSH $ip "kill \$(cat /mnt/nvme/netmon.pid) \$(cat /mnt/nvme/sampler.pid) 2>/dev/null"; $SCP $ip:/mnt/nvme/net.txt $R/raw/$tag-net$i.txt; $SCP $ip:/mnt/nvme/s.csv $R/raw/$tag-node$i.csv; $NSSH $ip "du -sb /mnt/nvme/wd | cut -f1; grep -c -i panicked /mnt/nvme/wd/node.out" > $R/raw/$tag-node$i.log; i=$((i+1)); done
  echo "$t0 $t1" > $R/raw/$tag-times.txt
  while IFS=, read phase bytes cl ops errs rate mean p50 p90 p99; do
    [ "$phase" = put ] || [ "$phase" = get ] || continue
    echo "$stage,$bin,$data,$arm,$size,$clients,$rep,$phase,$ops,$errs,$rate,$mean,$p50,$p90,$p99" >> $R/summary.csv
  done < $R/raw/$tag.csv
  log "$tag: $(grep -E '^(put|get),' $R/raw/$tag.csv | cut -d, -f1,4,5,6,8 | tr '\n' ' ') $(cat $R/raw/$tag-decisions.txt)"
}
rotate() {  # rotate <n> <items...>: the items, starting at index n mod count
  local n=$1; shift; local a=("$@") c=$#; for j in $(seq 0 $((c-1))); do echo -n "${a[$(( (n+j) % c ))]} "; done; }
for stage in $STAGES; do case $stage in
  sweep) for rep in $(seq 1 $REPS); do for data in $DATAS; do for spec in $SPECS; do IFS=: read size clients <<< "$spec"
           for arm in $(rotate $((rep-1)) off zstd auto); do run sweep $SWEEP_BIN $data $arm $size $clients $rep; done; done; done; done ;;
  native) pairs=""; for b in $BUILDS; do pairs+="$b:off $b:zstd "; done
         for rep in $(seq 1 $NREPS); do for spec in 65536:64 1048576:32; do IFS=: read size clients <<< "$spec"
           for pair in $(rotate $((rep-1)) $pairs); do IFS=: read bin arm <<< "$pair"
             run native $bin text $arm $size $clients $rep; done; done; done ;;
  clients) for c in $CLIENTS; do for arm in $(rotate $c off zstd auto); do run clients $SWEEP_BIN text $arm 1048576 $c 1; done; done ;;
esac; log "STAGE $stage DONE"; done
log "ZS3 DONE"
