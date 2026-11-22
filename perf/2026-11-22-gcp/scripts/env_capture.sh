#!/usr/bin/env bash
# env_capture.sh <out dir>: hardware and software of the client (this host) and every node in ~/ips.txt, one file each.
OUT=${1:?out dir}; mkdir -p $OUT
NSSH="ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -n"
probe='
md() { curl -s -H "Metadata-Flavor: Google" "http://metadata.google.internal/computeMetadata/v1/instance/$1"; }
echo "## host"; hostname; echo "machine_type: $(md machine-type | sed "s|.*/||")"; echo "zone: $(md zone | sed "s|.*/||")"; echo "cpu_platform: $(md cpu-platform)"
echo "captured_utc: $(date -u +%FT%TZ)"; echo "boot_utc: $(uptime -s)"
echo "## cpu"; lscpu | grep -E "^(Architecture|CPU\(s\)|Model name|Thread|Core|Socket|NUMA node\(s\)|CPU MHz|L1d|L2|L3|Flags)" | sed "s/Flags:.*avx512.*/Flags: (avx512 present)/"
echo "governor: $(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor 2>/dev/null || echo none)"
echo "## memory"; free -m | head -2
echo "## kernel and os"; uname -r; . /etc/os-release; echo "$PRETTY_NAME"
echo "## storage"; lsblk -d -o NAME,MODEL,SIZE,ROTA,TRAN | grep -v loop
for d in /sys/block/nvme0n1; do [ -e $d ] && echo "nvme0n1 scheduler: $(cat $d/queue/scheduler) nr_requests: $(cat $d/queue/nr_requests) logical_block: $(cat $d/queue/logical_block_size)"; done
lsblk -o NAME,SIZE,FSTYPE,MOUNTPOINT | grep -E "nvme|sda1"
echo "/mnt/nvme: $(findmnt -no SOURCE,FSTYPE,OPTIONS /mnt/nvme 2>/dev/null || echo not mounted)"
echo "## tunables"; for k in vm.dirty_ratio vm.dirty_background_ratio vm.swappiness net.core.somaxconn net.ipv4.tcp_congestion_control fs.file-max; do echo "$k = $(sysctl -n $k)"; done
echo "ulimit_nofile: $(ulimit -n)"
echo "## network"; ip -o link show | awk "{print \$2, \$5}" | grep -v "^lo"; ethtool -i ens4 2>/dev/null | head -2
echo "## binaries"; ls -l --time-style=+%FT%T ~/bins/ 2>/dev/null | awk "NR>1{print \$6, \$7}"; for b in ~/bins/wd-*; do [ -x $b ] && echo "$(basename $b) $(sha256sum $b | cut -c1-16)"; done
'
bash -c "$probe" > $OUT/client-wlc.txt 2>&1
{ echo "## toolchain (builds happen on wlc)"; . $HOME/.cargo/env; rustc -Vv; cargo -V; echo "## release profile"; sed -n '/^\[profile.release\]/,/^\[/p' ~/src-cgc/server/Cargo.toml 2>/dev/null | head -12; echo "## tarballs and commits"; for t in ~/src-*.tgz; do echo "$(basename $t) $(tar xzOf $t ./COMMIT 2>/dev/null || echo no-COMMIT-file)"; done; } >> $OUT/client-wlc.txt 2>&1
i=0; for ip in $(cat ~/ips.txt); do $NSSH $ip "$probe" > $OUT/node$i.txt 2>&1; i=$((i+1)); done
ls -la $OUT
