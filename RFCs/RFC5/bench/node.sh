#!/usr/bin/env bash
# node.sh start <binary> <run_id> [VAR=value ...]   |   node.sh stop <run_id>   |   node.sh syncs
set -u
PEERS="http://10.128.0.5:9710,http://10.128.0.6:9710,http://10.128.0.2:9710,http://10.128.0.3:9710,http://10.128.0.4:9710"
SELF=$(hostname -I | awk '{print $1}')
case "$1" in
start)
  BIN=$2; RUN=$3; shift 3
  pkill -f "bins/wd-" 2>/dev/null; pkill -x sar 2>/dev/null; pkill -x sadc 2>/dev/null; pkill -x iostat 2>/dev/null; sleep 0.5
  rm -rf /mnt/nvme/wd; mkdir -p /mnt/nvme/wd /mnt/nvme/mon/$RUN
  cp ~/bins/server_log.yaml /mnt/nvme/wd/
  ( cd /mnt/nvme/wd && env "$@" WARPDRIVE_PORT=9710 WARPDRIVE_SELF_ADDR=http://$SELF:9710 WARPDRIVE_PEERS=$PEERS \
      WARPDRIVE_RS_K=3 WARPDRIVE_RS_M=2 WARPDRIVE_ADMIN_ACCESS_KEY=AKIAIOSFODNN7EXAMPLE \
      WARPDRIVE_ADMIN_SECRET_KEY=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY setsid nohup ~/bins/$BIN > node.out 2>&1 < /dev/null & )
  setsid nohup sar -u -r -n DEV 1 > /mnt/nvme/mon/$RUN/sar.txt 2>&1 < /dev/null &
  setsid nohup iostat -dxmt 1 nvme0n1 > /mnt/nvme/mon/$RUN/iostat.txt 2>&1 < /dev/null &
  for i in $(seq 1 30); do curl -s -o /dev/null http://127.0.0.1:9710/ && break; sleep 0.5; done
  echo "$(hostname) up"
  ;;
stop)
  RUN=$2
  pkill -x sar 2>/dev/null; pkill -x sadc 2>/dev/null; pkill -x iostat 2>/dev/null
  curl -s http://127.0.0.1:9710/cluster/_internal/shard_timing > /mnt/nvme/mon/$RUN/shard_timing.txt
  curl -s http://127.0.0.1:9710/cluster/_internal/timing > /mnt/nvme/mon/$RUN/timing.txt
  grep -h "trace " /mnt/nvme/wd/logs/application.log > /mnt/nvme/mon/$RUN/traces.txt 2>/dev/null
  pkill -f "bins/wd-" 2>/dev/null
  du -sk /mnt/nvme/wd | awk '{print $1}' > /mnt/nvme/mon/$RUN/disk_kb.txt
  echo "$(hostname) stopped"
  ;;
syncs)
  curl -s http://127.0.0.1:9710/cluster/_internal/timing | sed -n 's/^sync_count=//p'
  ;;
esac
