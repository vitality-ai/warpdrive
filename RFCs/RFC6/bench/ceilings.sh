#!/usr/bin/env bash
# Device ceilings for the roofline, on the XFS partition. One JSON line per test
# in ~/results/ceilings.jsonl.
#   write_sync: 8 writers appending 1 MiB blocks with fdatasync after each, like segbench
#   write_nosync: same without syncs (upper bound)
#   read_direct_1m / read_direct_64k: O_DIRECT random reads, 4 jobs x iodepth 4
set -u
mkdir -p ~/results; D=/mnt/xfs/fio; mkdir -p $D
run() { name=$1; shift
  fio --name=$name --directory=$D --size=4g --runtime=30 --time_based --group_reporting --output-format=json "$@" > /tmp/$name.json 2>/dev/null
  python3 - "$name" /tmp/$name.json <<'PY' >> ~/results/ceilings.jsonl
import json, sys
j = json.load(open(sys.argv[2]))["jobs"][0]
r, w = j["read"], j["write"]
print(json.dumps({"test": sys.argv[1], "read_mbs": r["bw_bytes"] / 1e6, "read_iops": r["iops"],
                  "write_mbs": w["bw_bytes"] / 1e6, "write_iops": w["iops"],
                  "cpu_usr": j["usr_cpu"], "cpu_sys": j["sys_cpu"]}))
PY
  rm -f $D/*
}
run write_sync --rw=write --bs=1m --numjobs=8 --fdatasync=1 --ioengine=psync
run write_nosync --rw=write --bs=1m --numjobs=8 --ioengine=psync
run read_direct_1m --rw=randread --bs=1m --numjobs=4 --iodepth=4 --direct=1 --ioengine=libaio
run read_direct_64k --rw=randread --bs=64k --numjobs=4 --iodepth=4 --direct=1 --ioengine=libaio
echo "nproc=$(nproc)" >> ~/results/ceilings.txt
