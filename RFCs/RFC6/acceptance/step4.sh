#!/usr/bin/env bash
# RFC 6 step 4 acceptance on a local 3-node RS(2,1) cluster: the compactor frees dead shards' space
# (disk = blocks allocated, so punched holes count as freed),
# and every live object still reads back exactly, during compaction, after it, and after a restart.
S=${S:?set S to a scratch dir holding wt-rfc6, s3venv and server_log.yaml}
BIN=$S/wt-rfc6/server/target/release/warp_drive; R=$S/step4-cluster; rm -rf $R
PEERS="http://127.0.0.1:21910,http://127.0.0.1:21911,http://127.0.0.1:21912"
ENVS="WARPDRIVE_SEGMENT_MB=1 WARPDRIVE_RECLAIM_GRACE_SECS=3 WARPDRIVE_RECLAIM_INTERVAL_SECS=3600 WARPDRIVE_RECLAIM_CONFIRM_SECS=2 WARPDRIVE_COMPACT_SEAL_SECS=2 WARPDRIVE_COMPACT_UNLINK_SECS=1 WARPDRIVE_COMPACT_MBPS=200"
start() { i=$1; mkdir -p $R/n$i; cp $S/server_log.yaml $R/n$i/
  (cd $R/n$i && env $ENVS WARPDRIVE_PORT=2191$i WARPDRIVE_SELF_ADDR=http://127.0.0.1:2191$i WARPDRIVE_PEERS=$PEERS WARPDRIVE_RS_K=2 WARPDRIVE_RS_M=1 WARPDRIVE_ADMIN_ACCESS_KEY=AKIAIOSFODNN7EXAMPLE WARPDRIVE_ADMIN_SECRET_KEY=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY nohup $BIN >> node.out 2>&1 &)
  for k in $(seq 1 30); do curl -s -o /dev/null http://127.0.0.1:2191$i/ && break; sleep 0.5; done; }
stop() { i=$1; for d in 0 1000 2000; do lsof -ti tcp:$((21910+i+d)) -sTCP:LISTEN | xargs kill 2>/dev/null; done
  for k in $(seq 1 30); do busy=0; for d in 0 1000 2000; do lsof -ti tcp:$((21910+i+d)) -sTCP:LISTEN >/dev/null && busy=1; done; [ $busy = 0 ] && break; sleep 0.5; done; }
for i in 0 1 2; do start $i; done; sleep 2
R=$R $S/s3venv/bin/python - <<'PY'
import boto3, json, os, random, subprocess, threading, time, urllib.request
from botocore.config import Config
R=os.environ["R"]
c=boto3.client("s3",endpoint_url="http://127.0.0.1:21910",aws_access_key_id="AKIAIOSFODNN7EXAMPLE",aws_secret_access_key="wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",region_name="us-east-1",config=Config(s3={"addressing_style":"path"},max_pool_connections=32))
def disk():
    out=[]
    for i in range(3):
        n=0; segs=0
        for root,_,files in os.walk(f"{R}/n{i}/storage"):
            for f in files:
                if f.endswith(".seg"): n+=os.stat(os.path.join(root,f)).st_blocks*512; segs+=1
        out.append((n,segs))
    return out
def run(i): return json.loads(urllib.request.urlopen(urllib.request.Request(f"http://127.0.0.1:2191{i}/cluster/_internal/reclaim/run",method="POST"),timeout=600).read())
c.create_bucket(Bucket="cmp")
random.seed(7)
want={}
def body(k,gen,n): return (f"{k}-{gen}-".encode()*(n//8+1))[:n]
sizes={f"o{i}": random.choice([4096, 20_000, 100_000, 300_000]) for i in range(400)}
for k,n in sizes.items(): c.put_object(Bucket="cmp",Key=k,Body=body(k,0,n)); want[k]=body(k,0,n)
keys=list(sizes)
for k in keys[:250]: c.put_object(Bucket="cmp",Key=k,Body=body(k,1,sizes[k])); want[k]=body(k,1,sizes[k])
for k in keys[250:330]: c.delete_object(Bucket="cmp",Key=k); del want[k]
live=sum(len(v) for v in want.values())
before=disk()
print(f"live object bytes {live}  (RS(2,1): about {live//2} per node)")
print("before:", [f"{b/1e6:.1f} MB in {s} segs" for b,s in before])
time.sleep(4)  # past the reclaim grace and the seal idle time
# a writer and a reader run through the compaction
stop_flag=False; errs=[]; extra={}
def writer():
    i=0
    while not stop_flag:
        k=f"during{i}"; b=body(k,0,30_000)
        try: c.put_object(Bucket="cmp",Key=k,Body=b); extra[k]=b
        except Exception as e: errs.append(f"put {type(e).__name__}: {e}"[:200])
        i+=1
def reader():
    ks=list(want)
    while not stop_flag:
        k=random.choice(ks)
        try:
            got=c.get_object(Bucket="cmp",Key=k)["Body"].read()
            if got!=want[k]: errs.append(f"get {k}: wrong bytes")
        except Exception as e: errs.append(f"get {k} {type(e).__name__}: {e}"[:200])
ts=[threading.Thread(target=writer)]+[threading.Thread(target=reader) for _ in range(4)]
[t.start() for t in ts]
first=[run(i) for i in range(3)]          # first sweep: nothing is freed yet
print("first run, node0:", first[0]["confirmed_dead"], "confirmed dead,", first[0]["compact"])
time.sleep(3)
second=[run(i) for i in range(3)]         # second sweep: dead in both, freed
for i,r in enumerate(second): print(f"second run, node{i}: sweep reclaimable {r['sweep']['reclaimable_superseded']+r['sweep']['reclaimable_confirmed']}, confirmed {r['confirmed_dead']}, compact {r['compact']}")
time.sleep(2)
third=[run(i) for i in range(3)]          # deletes the segments past their unlink grace
for i,r in enumerate(third): print(f"third run, node{i}: compact {r['compact']}")
stop_flag=True; [t.join() for t in ts]
after=disk()
print("after: ", [f"{b/1e6:.1f} MB in {s} segs" for b,s in after])
print("freed: ", [f"{(b0-b1)/1e6:.1f} MB ({100*(b0-b1)/b0:.0f}%)" for (b0,_),(b1,_) in zip(before,after)])
print("errors during compaction:", len(errs), errs[:3])
want.update(extra)
bad=[k for k,v in want.items() if c.get_object(Bucket="cmp",Key=k)["Body"].read()!=v]
print(f"after compaction: {len(want)-len(bad)}/{len(want)} objects read back exactly")
gone=[k for k in keys[250:330] if k in [o["Key"] for p in c.get_paginator("list_objects_v2").paginate(Bucket="cmp") for o in p.get("Contents",[])]]
print("deleted keys listed again:", len(gone))
json.dump({k: v.hex() for k,v in want.items()}, open(f"{R}/want.json","w"))
PY
echo "== restart every node, read everything again"
for i in 0 1 2; do stop $i; done; for i in 0 1 2; do start $i; done; sleep 2
R=$R $S/s3venv/bin/python - <<'PY'
import boto3, json, os
from botocore.config import Config
c=boto3.client("s3",endpoint_url="http://127.0.0.1:21910",aws_access_key_id="AKIAIOSFODNN7EXAMPLE",aws_secret_access_key="wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",region_name="us-east-1",config=Config(s3={"addressing_style":"path"}))
want={k: bytes.fromhex(v) for k,v in json.load(open(os.environ["R"]+"/want.json")).items()}
bad=[k for k,v in want.items() if c.get_object(Bucket="cmp",Key=k)["Body"].read()!=v]
print(f"after restart: {len(want)-len(bad)}/{len(want)} objects read back exactly", bad[:3])
PY
for i in 0 1 2; do stop $i; done; grep -h "panicked\|ERROR\|still has" $R/*/node.out | head -5
