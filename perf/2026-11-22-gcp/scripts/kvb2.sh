#!/usr/bin/env bash
# Runs on wlc: RocksDB read-side options on wln-1, 1 M rows, same sweep (baseline rerun alongside).
R=~/kvb2; rm -rf $R; mkdir -p $R; log() { echo "$(date -u +%T) $*" >> $R/progress.log; }
IPS=($(cat ~/ips.txt)); NSSH="ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -n"
SCP="scp -q -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
. ~/.cargo/env
rm -rf ~/kvbench; tar xzf ~/kvbench.tgz -C ~ 2>/dev/null
sed -i "s#^warp-meta-store = .*#warp-meta-store = { path = \"/home/cj/src-final3/crates/warp-meta-store\" }#" ~/kvbench/Cargo.toml
(cd ~/kvbench && CARGO_TARGET_DIR=~/tgt-kvbench cargo build --release 2>&1 | tail -2) > $R/build.log
grep -q Finished $R/build.log || { log "ABORT build failed"; exit 1; }
N=${IPS[1]}; $SCP ~/tgt-kvbench/release/kvbench $N:~/bins/kvbench
log "bin: kvbench $(sha256sum ~/tgt-kvbench/release/kvbench | cut -c1-16)"
run() { name=$1; shift; log "start $name"
  $NSSH $N "sync; rm -rf /mnt/nvme/kvb-r; export $*; ~/bins/kvbench rocksdb /mnt/nvme/kvb-r 1000000 \${KVB_THREADS:-64} 30 \${KVB_CACHE:-256} 2>&1; du -sh /mnt/nvme/kvb-r; rm -rf /mnt/nvme/kvb-r" > $R/$name.out 2>&1
  log "done $name: $(grep -E '^(load,rocksdb,1000000|get,rocksdb|scan,rocksdb|reopen,rocksdb)' $R/$name.out | tr '\n' ' ')"; }
run A_baseline KVB_X=1
run B_cache1g KVB_CACHE=1024
run C_rowcache KVB_ROW_CACHE_MB=256
run D_hashindex KVB_HASH_INDEX=1
run E_nocompress KVB_NO_COMPRESSION=1
run F_16threads KVB_THREADS=16
log "ALL DONE"
