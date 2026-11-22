#!/usr/bin/env bash
# RFC 6 step 4 acceptance, hole punching: segments that stay above umax get their dead whole blocks
# punched in place. Large objects with every 4th overwritten keep sealed segments ~75% live.
S=${S:?set S to a scratch dir holding wt-rfc6, s3venv and server_log.yaml}
BIN=$S/wt-rfc6/server/target/release/warp_drive; R=$S/step4p-cluster; rm -rf $R
PEERS="http://127.0.0.1:21910,http://127.0.0.1:21911,http://127.0.0.1:21912"
ENVS="WARPDRIVE_SEGMENT_MB=1 WARPDRIVE_RECLAIM_GRACE_SECS=3 WARPDRIVE_RECLAIM_INTERVAL_SECS=3600 WARPDRIVE_RECLAIM_CONFIRM_SECS=2 WARPDRIVE_COMPACT_SEAL_SECS=2 WARPDRIVE_COMPACT_UNLINK_SECS=1 WARPDRIVE_COMPACT_MBPS=200"
start() { i=$1; mkdir -p $R/n$i; cp $S/server_log.yaml $R/n$i/
  (cd $R/n$i && env $ENVS WARPDRIVE_PORT=2191$i WARPDRIVE_SELF_ADDR=http://127.0.0.1:2191$i WARPDRIVE_PEERS=$PEERS WARPDRIVE_RS_K=2 WARPDRIVE_RS_M=1 WARPDRIVE_ADMIN_ACCESS_KEY=AKIAIOSFODNN7EXAMPLE WARPDRIVE_ADMIN_SECRET_KEY=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY nohup $BIN >> node.out 2>&1 &)
  for k in $(seq 1 30); do curl -s -o /dev/null http://127.0.0.1:2191$i/ && break; sleep 0.5; done; }
stop() { i=$1; for d in 0 1000 2000; do lsof -ti tcp:$((21910+i+d)) -sTCP:LISTEN | xargs kill 2>/dev/null; done
  for k in $(seq 1 30); do busy=0; for d in 0 1000 2000; do lsof -ti tcp:$((21910+i+d)) -sTCP:LISTEN >/dev/null && busy=1; done; [ $busy = 0 ] && break; sleep 0.5; done; }
for i in 0 1 2; do start $i; done; sleep 2
R=$R $S/s3venv/bin/python - <<'PY'
import boto3, json, os, time, urllib.request, threading, random
from botocore.config import Config
R=os.environ["R"]
c=boto3.client("s3",endpoint_url="http://127.0.0.1:21910",aws_access_key_id="AKIAIOSFODNN7EXAMPLE",aws_secret_access_key="wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",region_name="us-east-1",config=Config(s3={"addressing_style":"path"},max_pool_connections=16))
def disk():
    out=[]
    for i in range(3):
        phys=app=0
        for root,_,files in os.walk(f"{R}/n{i}/storage"):
            for f in files:
                if f.endswith(".seg"): st=os.stat(os.path.join(root,f)); phys+=st.st_blocks*512; app+=st.st_size
        out.append((phys,app))
    return out
def run(i): return json.loads(urllib.request.urlopen(urllib.request.Request(f"http://127.0.0.1:2191{i}/cluster/_internal/reclaim/run",method="POST"),timeout=600).read())
c.create_bucket(Bucket="pun")
N=200; SZ=200_000
body=lambda k,g: (f"{k}-{g}-".encode()*(SZ//6+1))[:SZ]
want={f"p{i}": body(f"p{i}",0) for i in range(N)}
for k,v in want.items(): c.put_object(Bucket="pun",Key=k,Body=v)
for i in range(0,N,4): k=f"p{i}"; want[k]=body(k,1); c.put_object(Bucket="pun",Key=k,Body=want[k])
c.put_object(Bucket="pun",Key="seal",Body=b"s"*1_100_000)  # rolls the fresh head past the last segment written
want["seal"]=b"s"*1_100_000
print("before (physical, apparent) MB:", [(round(p/1e6,1),round(a/1e6,1)) for p,a in disk()])
time.sleep(4)
stop=False; errs=[]
def reader():
    ks=list(want)
    while not stop:
        k=random.choice(ks)
        try:
            if c.get_object(Bucket="pun",Key=k)["Body"].read()!=want[k]: errs.append(f"{k}: wrong bytes")
        except Exception as e: errs.append(f"{k} {type(e).__name__}")
ts=[threading.Thread(target=reader) for _ in range(4)]; [t.start() for t in ts]
for i in range(3): run(i)
time.sleep(3)
second=[run(i) for i in range(3)]
for i,r in enumerate(second): print(f"node{i}: confirmed {r['confirmed_dead']}, compacted {r['compact']['segments_compacted']}, punched {r['compact']['bytes_punched']/1e6:.1f} MB")
time.sleep(2); [run(i) for i in range(3)]
stop=True; [t.join() for t in ts]
after=disk()
print("after  (physical, apparent) MB:", [(round(p/1e6,1),round(a/1e6,1)) for p,a in after])
print("errors during punching:", len(errs), errs[:3])
bad=[k for k,v in want.items() if c.get_object(Bucket="pun",Key=k)["Body"].read()!=v]
print(f"after: {len(want)-len(bad)}/{len(want)} objects read back exactly", bad[:3])
json.dump({k: v.hex() for k,v in want.items()}, open(f"{R}/want.json","w"))
PY
for i in 0 1 2; do stop $i; done; for i in 0 1 2; do start $i; done; sleep 2
R=$R $S/s3venv/bin/python - <<'PY'
import boto3, json, os
from botocore.config import Config
c=boto3.client("s3",endpoint_url="http://127.0.0.1:21910",aws_access_key_id="AKIAIOSFODNN7EXAMPLE",aws_secret_access_key="wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",region_name="us-east-1",config=Config(s3={"addressing_style":"path"}))
want={k: bytes.fromhex(v) for k,v in json.load(open(os.environ["R"]+"/want.json")).items()}
bad=[k for k,v in want.items() if c.get_object(Bucket="pun",Key=k)["Body"].read()!=v]
print(f"after restart: {len(want)-len(bad)}/{len(want)} objects read back exactly", bad[:3])
PY
for i in 0 1 2; do stop $i; done; grep -rh "panicked\|punching segment" $R | head -3
