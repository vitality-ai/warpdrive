#!/usr/bin/env python3
# Author: Tejus Chandrashekar
"""
Local orchestrator for real-network-partition chaos testing: drives
nemesis actions against a real 5-node GCP cluster over SSH (gcloud
compute ssh / IAP tunnel) while gcp_workload.py runs the client workload
on node-0 against its own localhost:9710. Same nemesis action set as
jepsen_style_test.py (kill/restart/corrupt), plus a real network-partition
action (iptables DROP rules between two real machines) that a single
local machine cannot produce -- this is the concrete capability real
infrastructure unlocks over the local-only harness. See
docs/CHAOS-TESTING.md for the full writeup, methodology, and results.

Assumes 5 VMs already exist (named chaos-node-0..4, internal IPs below)
with the warp_drive release binary at ~/warpdrive/server/warp_drive and
server_log.yaml alongside it, and passwordless sudo for iptables. The
zone/names/IPs are hardcoded for the specific GCP project this was run
against -- adjust them (or the VM creation commands in
docs/CHAOS-TESTING.md's "Reproducing on GCP" section) for a different
project.

Usage:
    python3 gcp_orchestrator.py [duration_seconds]   # default 180
    python3 gcp_checker.py                            # analyze the result
"""
import json
import random
import subprocess
import sys
import time

ZONE = "us-central1-a"
NAMES = {i: f"chaos-node-{i}" for i in range(5)}
IPS = {0: "10.128.0.5", 1: "10.128.0.3", 2: "10.128.0.4", 3: "10.128.0.2", 4: "10.128.0.6"}
PEERS = ",".join(f"http://{IPS[i]}:9710" for i in range(5))
DURATION_SEC = int(sys.argv[1]) if len(sys.argv) > 1 else 180

START_CMD = f"""
mkdir -p ~/warpdrive/server/storage
cd ~/warpdrive/server
pkill -9 warp_drive 2>/dev/null
sudo iptables -F 2>/dev/null
WARPDRIVE_PORT=9710 WARPDRIVE_PEERS='{PEERS}' WARPDRIVE_RS_K=3 WARPDRIVE_RS_M=2 \
WARPDRIVE_LOCATION_LOG=location.log WARPDRIVE_CONTENT_LOCATION_LOG=content_location.log \
WARPDRIVE_BUCKET_CONFIG_LOG=bucket_config.log WARPDRIVE_SHARD_META_LOG=shard_meta.log \
STORAGE_DIRECTORY=storage WARPDRIVE_ADMIN_ACCESS_KEY=AKIAIOSFODNN7EXAMPLE \
WARPDRIVE_ADMIN_SECRET_KEY=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY RUST_LOG=warn \
nohup ./warp_drive > stdout.log 2>&1 &
disown
sleep 1
"""


def ssh(i, cmd, timeout=30):
    full = ["gcloud", "compute", "ssh", NAMES[i], f"--zone={ZONE}", "--tunnel-through-iap", f"--command={cmd}"]
    try:
        r = subprocess.run(full, capture_output=True, text=True, timeout=timeout)
        return r.returncode, r.stdout, r.stderr
    except subprocess.TimeoutExpired:
        return -1, "", "timeout"


def kill_node(i):
    ssh(i, "pkill -9 warp_drive 2>/dev/null; echo ok")


def start_node(i):
    ssh(i, START_CMD, timeout=30)


def wait_healthy(i, timeout=15):
    deadline = time.time() + timeout
    while time.time() < deadline:
        rc, out, err = ssh(i, "curl -sf http://localhost:9710/cluster/_internal/timing >/dev/null && echo OK", timeout=10)
        if "OK" in out:
            return True
        time.sleep(1)
    return False


def partition_node(i, target_ips):
    rules = " ; ".join(
        f"sudo iptables -A INPUT -s {ip} -j DROP ; sudo iptables -A OUTPUT -d {ip} -j DROP"
        for ip in target_ips
    )
    ssh(i, rules, timeout=20)


def heal_node(i):
    ssh(i, "sudo iptables -F", timeout=20)


def corrupt_shard(i, bucket):
    script = (
        "python3 -c \""
        "import os,random\n"
        f"d=os.path.expanduser('~/warpdrive/server/storage/admin')\n"
        "import glob\n"
        f"cands=[p for p in glob.glob(d+'/*') if '{bucket}' in p]\n"
        "if cands:\n"
        "    p=cands[0]\n"
        "    data=bytearray(open(p,'rb').read())\n"
        "    if len(data)>32:\n"
        "        pos=random.randint(0,len(data)-1)\n"
        "        data[pos]^=0xFF\n"
        "        open(p,'wb').write(bytes(data))\n"
        "        print('corrupted', p, 'at', pos)\n"
        "\""
    )
    rc, out, err = ssh(i, script, timeout=20)
    return out.strip()


def main():
    print("=== resetting and starting all 5 real GCP nodes ===")
    for i in range(5):
        heal_node(i)
        start_node(i)
    for i in range(5):
        if not wait_healthy(i):
            print(f"node {i} failed to become healthy")
            sys.exit(1)
    print("all healthy")

    print(f"=== deploying + launching workload on node-0 for {DURATION_SEC}s ===")
    subprocess.run(["gcloud", "compute", "scp", "/tmp/gcp_workload.py", "chaos-node-0:~/gcp_workload.py",
                    f"--zone={ZONE}", "--tunnel-through-iap"], capture_output=True, text=True, timeout=60)
    subprocess.run(["gcloud", "compute", "ssh", "chaos-node-0", f"--zone={ZONE}", "--tunnel-through-iap",
                    "--command", f"rm -f ~/gcp_jepsen_history.json; nohup ~/venv/bin/python3 ~/gcp_workload.py {DURATION_SEC} > ~/workload.log 2>&1 & disown; sleep 1; echo launched"],
                   capture_output=True, text=True, timeout=30)

    nemesis_log = []
    down = set()
    partitioned = set()  # set of node indices currently partitioned from someone
    start_time = time.time()
    buffer_sec = 20  # let the workload run a bit before/after nemesis window for clean baseline
    time.sleep(5)

    while time.time() - start_time < DURATION_SEC - buffer_sec:
        time.sleep(random.uniform(3.0, 7.0))
        up = [i for i in range(1, 5) if i not in down]  # never kill node-0 (runs the workload too)
        partition_candidates = [i for i in range(5) if i not in down]  # node-0 CAN be partitioned (process stays alive)
        choices = []
        if up:
            choices.append("kill")
        if down:
            choices.append("restart")
        if len(partition_candidates) >= 2:
            choices.append("partition")
        if partitioned:
            choices.append("heal")
        choices.append("corrupt")
        action = random.choice(choices)
        t0 = time.time()
        if action == "kill":
            victim = random.choice(up)
            kill_node(victim)
            down.add(victim)
            nemesis_log.append({"t": t0, "action": "kill", "target": victim})
            print(f"  t={t0-start_time:.1f}s NEMESIS kill node {victim}")
        elif action == "restart":
            victim = random.choice(list(down))
            start_node(victim)
            healthy = wait_healthy(victim)
            down.discard(victim)
            nemesis_log.append({"t": t0, "action": "restart", "target": victim, "healthy": healthy})
            print(f"  t={t0-start_time:.1f}s NEMESIS restart node {victim} healthy={healthy}")
        elif action == "partition":
            src = random.choice(partition_candidates)
            others = [j for j in partition_candidates if j != src]
            n_targets = random.choice([1, 2])
            targets = random.sample(others, min(n_targets, len(others)))
            partition_node(src, [IPS[t] for t in targets])
            partitioned.add(src)
            nemesis_log.append({"t": t0, "action": "partition", "source": src, "targets": targets})
            print(f"  t={t0-start_time:.1f}s NEMESIS partition node {src} from {targets}")
        elif action == "heal":
            victim = random.choice(list(partitioned))
            heal_node(victim)
            partitioned.discard(victim)
            nemesis_log.append({"t": t0, "action": "heal", "target": victim})
            print(f"  t={t0-start_time:.1f}s NEMESIS heal node {victim}")
        elif action == "corrupt":
            detail = corrupt_shard(0, "jepsen-gcp-bucket")
            nemesis_log.append({"t": t0, "action": "corrupt", "detail": detail})
            print(f"  t={t0-start_time:.1f}s NEMESIS corrupt: {detail}")

    print("=== nemesis window done, healing + restarting everything ===")
    for i in list(partitioned):
        heal_node(i)
    for i in list(down):
        start_node(i)
        wait_healthy(i)

    print("=== waiting for workload to finish ===")
    remaining = DURATION_SEC - (time.time() - start_time) + 10
    time.sleep(max(remaining, 5))

    subprocess.run(["gcloud", "compute", "scp", "chaos-node-0:~/gcp_jepsen_history.json", "/tmp/gcp_jepsen_history.json",
                    f"--zone={ZONE}", "--tunnel-through-iap"], capture_output=True, text=True, timeout=60)
    with open("/tmp/gcp_nemesis_log.json", "w") as f:
        json.dump(nemesis_log, f)

    print(f"=== done. history -> /tmp/gcp_jepsen_history.json, nemesis log -> /tmp/gcp_nemesis_log.json ({len(nemesis_log)} events) ===")


if __name__ == "__main__":
    main()
