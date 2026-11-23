#!/usr/bin/env bash
# Runs on wlc: engine benchmark (kvbench) of WarpMetaStore's Bitcask engine vs RocksDB on wln-1 (n2-standard-2, 8 GB,
# local NVMe ext4). 1 M rows of ~816 bytes from 64 writers (durable writes), 30 s random GETs, 200 prefix scans, reopen.
R=~/kvb; rm -rf $R; mkdir -p $R; log() { echo "$(date -u +%T) $*" >> $R/progress.log; }
IPS=($(cat ~/ips.txt)); NSSH="ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -n"
SCP="scp -q -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
until grep -q "BUILD-EXIT" ~/kvbench-build.log; do sleep 10; done
grep -q "BUILD-EXIT 0" ~/kvbench-build.log || { log "ABORT build failed"; exit 1; }
N=${IPS[1]}
for i in 0 1 2; do $NSSH ${IPS[$i]} "pkill -x wd-final3; pkill -f '[s]ampler.sh'"; done
$NSSH $N "mountpoint -q /mnt/nvme" || { log "ABORT no nvme"; exit 1; }
$SCP ~/tgt-kvbench/release/kvbench $N:~/bins/kvbench
log "bin: kvbench $(sha256sum ~/tgt-kvbench/release/kvbench | cut -c1-16), warp-meta-store at $(cat ~/src-final3/COMMIT)"
$NSSH $N "sleep 5; rm -rf /mnt/nvme/wd /mnt/nvme/kvb; free -m | sed -n 2p; nproc" >> $R/progress.log
for e in rocksdb bitcask; do
  log "start $e"
  $NSSH $N "sync; ~/bins/kvbench $e /mnt/nvme/kvb-$e 1000000 64 30 256 2>&1; du -sh /mnt/nvme/kvb-$e; rm -rf /mnt/nvme/kvb-$e" > $R/$e.out 2>&1
  log "done $e: $(grep -E '^(get|scan|reopen),' $R/$e.out | grep -v engine | tr '\n' ' ')"
done
log "ALL DONE"
