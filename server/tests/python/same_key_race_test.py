"""Live: two clients PUT the same key at the same time, both through node 0.
After each round, compare the version every node holds for the key."""
import json, os, shutil, signal, subprocess, sys, threading, time, requests
BIN, RUN = sys.argv[1], sys.argv[2]
BASE, N = 20210, 5
NODES = [f"http://127.0.0.1:{BASE+i}" for i in range(N)]
procs = []
shutil.rmtree(RUN, ignore_errors=True)
for i in range(N):
    d = f"{RUN}/node{i}"; os.makedirs(d)
    shutil.copy(os.path.join(os.path.dirname(BIN), "../../server_log.yaml"), d)
    env = dict(os.environ, WARPDRIVE_PORT=str(BASE+i), WARPDRIVE_SELF_ADDR=NODES[i], WARPDRIVE_PEERS=",".join(NODES), WARPDRIVE_RS_K="3", WARPDRIVE_RS_M="2")
    procs.append(subprocess.Popen([BIN], cwd=d, env=env, stdout=open(f"{d}/node.out","w"), stderr=subprocess.STDOUT))
for i in range(N):
    for _ in range(300):
        try:
            requests.get(f"{NODES[i]}/cluster/_internal/timing", timeout=0.5); break
        except Exception: time.sleep(0.1)

def versions(key):
    out = []
    for i in range(N):
        v = None
        for l in open(f"{RUN}/node{i}/cluster_location.log"):
            rk, rec = json.loads(l)
            if rk.endswith(":" + key):
                if isinstance(rec, dict) and "Live" in rec: rec = rec["Live"]
                elif isinstance(rec, dict) and "Tombstone" in rec: rec = None
                v = rec["version"] if rec else None
        out.append(v)
    return out

rounds, split = int(sys.argv[3]), 0
try:
    for r in range(rounds):
        key = f"k{r}"
        bodies = [os.urandom(64 * 1024), os.urandom(64 * 1024)]
        go = threading.Barrier(2)
        def put(b):
            go.wait()
            requests.put(f"{NODES[0]}/cluster/race/{key}", data=b, timeout=30)
        ts = [threading.Thread(target=put, args=(b,)) for b in bodies]
        for t in ts: t.start()
        for t in ts: t.join()
        v = versions(key)
        if len(set(v)) > 1:
            split += 1
            if split <= 3: print(f"round {r}: nodes disagree: {v}")
    print(f"{split}/{rounds} rounds left nodes disagreeing on the key's version (both writes through node 0)")
finally:
    for p in procs: p.send_signal(signal.SIGTERM)
