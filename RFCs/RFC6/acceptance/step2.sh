#!/usr/bin/env bash
S=${S:?set S to a scratch dir holding wt-rfc6, s3venv and server_log.yaml}
BIN=$S/wt-rfc6/server/target/release/warp_drive; R=$S/step2-cluster; rm -rf $R
PEERS="http://127.0.0.1:21910,http://127.0.0.1:21911,http://127.0.0.1:21912"
start() { i=$1; mkdir -p $R/n$i; cp $S/server_log.yaml $R/n$i/
  (cd $R/n$i && WARPDRIVE_PORT=2191$i WARPDRIVE_SELF_ADDR=http://127.0.0.1:2191$i WARPDRIVE_PEERS=$PEERS WARPDRIVE_RS_K=2 WARPDRIVE_RS_M=1 WARPDRIVE_ADMIN_ACCESS_KEY=AKIAIOSFODNN7EXAMPLE WARPDRIVE_ADMIN_SECRET_KEY=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY nohup $BIN >> node.out 2>&1 &)
  for k in $(seq 1 30); do curl -s -o /dev/null http://127.0.0.1:2191$i/ && break; sleep 0.5; done; }
stop() { i=$1; for d in 0 1000 2000; do lsof -ti tcp:$((21910+i+d)) -sTCP:LISTEN | xargs kill 2>/dev/null; done
  for k in $(seq 1 30); do busy=0; for d in 0 1000 2000; do lsof -ti tcp:$((21910+i+d)) -sTCP:LISTEN >/dev/null && busy=1; done; [ $busy = 0 ] && break; sleep 0.5; done; }
for i in 0 1 2; do start $i; done; sleep 2
$S/s3venv/bin/python - <<'PY'
import boto3, threading
from botocore.config import Config
c=lambda: boto3.client("s3",endpoint_url="http://127.0.0.1:21910",aws_access_key_id="AKIAIOSFODNN7EXAMPLE",aws_secret_access_key="wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",region_name="us-east-1",config=Config(s3={"addressing_style":"path"},max_pool_connections=32))
c().create_bucket(Bucket="logs")
errs=[]
def w(t):
    cc=c()
    for r in range(300):
        for k in range(t,50,16):
            try: cc.put_object(Bucket="logs",Key=f"k{k}",Body=f"{k}-{r}".encode()*100)
            except Exception as e: errs.append(f"{type(e).__name__}: {str(e)[:150]}")
ts=[threading.Thread(target=w,args=(t,)) for t in range(16)]; [x.start() for x in ts]; [x.join() for x in ts]
from collections import Counter
print("overwrite PUTs:", 300*50, "errors:", len(errs), Counter(errs).most_common(3))
PY
echo "before: metadata log $(wc -l < $R/n0/cluster_metadata.log) lines, $(du -k $R/n0/cluster_metadata.log | cut -f1) KB"
# compact while PUTs to other keys keep running
$S/s3venv/bin/python - <<'PY' &
import boto3, time
from botocore.config import Config
cc=boto3.client("s3",endpoint_url="http://127.0.0.1:21910",aws_access_key_id="AKIAIOSFODNN7EXAMPLE",aws_secret_access_key="wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",region_name="us-east-1",config=Config(s3={"addressing_style":"path"}))
worst=0; errs=0; t_end=time.time()+6
i=0
while time.time()<t_end:
    t0=time.time()
    try: cc.put_object(Bucket="logs",Key=f"during{i}",Body=b"x"*100)
    except Exception: errs+=1
    worst=max(worst,time.time()-t0); i+=1
print(f"PUTs during compaction: {i}, errors {errs}, slowest {worst*1000:.0f} ms")
PY
sleep 1; echo "compact_logs on node0: $(curl -s -X POST http://127.0.0.1:21910/cluster/_internal/compact_logs)"; wait
echo "after:  metadata log $(wc -l < $R/n0/cluster_metadata.log) lines, $(du -k $R/n0/cluster_metadata.log | cut -f1) KB"
stop 0; start 0; sleep 1
$S/s3venv/bin/python - <<'PY'
import boto3
from botocore.config import Config
cc=boto3.client("s3",endpoint_url="http://127.0.0.1:21910",aws_access_key_id="AKIAIOSFODNN7EXAMPLE",aws_secret_access_key="wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",region_name="us-east-1",config=Config(s3={"addressing_style":"path"}))
last=299
ok=sum(cc.get_object(Bucket="logs",Key=f"k{k}")["Body"].read()==f"{k}-{last}".encode()*100 for k in range(50))
n=len([o for p in cc.get_paginator("list_objects_v2").paginate(Bucket="logs") for o in p.get("Contents",[])])
print(f"after restart on the compacted log: {ok}/50 keys hold their latest version, {n} objects listed")
PY
for i in 0 1 2; do stop $i; done; grep -h "panicked\|ERROR" $R/*/node.out | head -3; rm -rf $R
