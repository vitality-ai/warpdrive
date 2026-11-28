#!/usr/bin/env bash
# geo_netem.sh on|off, run on wlc. Adds the Azure geo run's round trips (perf/2026-11-28-azure-geo, TCP connect
# time, best of 5) between the GCP VMs with tc netem: each host delays packets to each other host by half the
# measured RTT, so a round trip adds the full RTT. Nodes in ~/ips.txt order map to Central India (node 0), Qatar,
# East Asia, Japan, Korea. wlc plays the South India client.
set -u
IPS=($(cat ~/ips.txt)); ME=$(hostname -I | awk '{print $1}')
NSSH="ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -n"
# RTT ms, rows from, columns to: node0..node4, then the client as the last row and column.
RTT=(
"0 39.8 76.5 116.0 107.5 15.8"
"39.9 0 122.0 164.3 154.9 61.3"
"76.2 121.9 0 46.0 35.8 68.2"
"115.7 161.9 46.3 0 26.2 106.0"
"106.5 152.2 35.7 23.4 0 94.0"
"15.8 61.3 68.2 106.0 94.0 0"
)
HOSTS=("${IPS[@]}" "$ME")
rules() {  # rules <row>: the tc commands for host <row>
  local row=$1 j=0 cmds="IF=\$(ip -o -4 route show to default | awk '{print \$5}'); sudo tc qdisc del dev \$IF root 2>/dev/null;"
  [ "$MODE" = off ] && { echo "$cmds"; return; }
  cmds+=" sudo tc qdisc add dev \$IF root handle 1: htb default 99; sudo tc class add dev \$IF parent 1: classid 1:99 htb rate 100gbit;"
  for d in ${RTT[$row]}; do
    if [ $j != $row ]; then
      local half=$(awk -v d=$d 'BEGIN{printf "%.2f", d/2}') c=$((10+j))
      cmds+=" sudo tc class add dev \$IF parent 1: classid 1:$c htb rate 100gbit; sudo tc qdisc add dev \$IF parent 1:$c handle $c: netem delay ${half}ms limit 1000000; sudo tc filter add dev \$IF protocol ip parent 1: prio 1 u32 match ip dst ${HOSTS[$j]}/32 flowid 1:$c;"
    fi
    j=$((j+1))
  done
  echo "$cmds"
}
MODE=${1:?on or off}
for i in 0 1 2 3 4; do $NSSH ${IPS[$i]} "$(rules $i)" || echo "FAILED on node $i"; done
bash -c "$(rules 5)" || echo "FAILED on client"
# Check: TCP connect time from here and from node 0.
for ip in "${IPS[@]}"; do echo -n "client->$ip $(curl -s -o /dev/null -m 5 -w '%{time_connect}' http://$ip:9710/ 2>/dev/null) "; done; echo
