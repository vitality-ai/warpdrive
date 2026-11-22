#!/usr/bin/env bash
# RFC 6 step 1 acceptance on a local 3-node RS(2,1) cluster.
S=${S:?set S to a scratch dir holding wt-rfc6, s3venv and server_log.yaml}
BIN=$S/wt-rfc6/server/target/release/warp_drive; R=$S/step1-cluster; rm -rf $R
PEERS="http://127.0.0.1:21910,http://127.0.0.1:21911,http://127.0.0.1:21912"
start() { i=$1; mkdir -p $R/n$i; cp $S/server_log.yaml $R/n$i/
  (cd $R/n$i && WARPDRIVE_RECLAIM_GRACE_SECS=3 WARPDRIVE_RECLAIM_INTERVAL_SECS=3600 WARPDRIVE_PORT=2191$i WARPDRIVE_SELF_ADDR=http://127.0.0.1:2191$i WARPDRIVE_PEERS=$PEERS WARPDRIVE_RS_K=2 WARPDRIVE_RS_M=1 WARPDRIVE_ADMIN_ACCESS_KEY=AKIAIOSFODNN7EXAMPLE WARPDRIVE_ADMIN_SECRET_KEY=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY nohup $BIN >> node.out 2>&1 &)
  for k in $(seq 1 30); do curl -s -o /dev/null http://127.0.0.1:2191$i/ && break; sleep 0.5; done; }
stop() { i=$1; for d in 0 1000 2000; do lsof -ti tcp:$((21910+i+d)) -sTCP:LISTEN | xargs kill 2>/dev/null; done
  for k in $(seq 1 30); do busy=0; for d in 0 1000 2000; do lsof -ti tcp:$((21910+i+d)) -sTCP:LISTEN >/dev/null && busy=1; done; [ $busy = 0 ] && break; sleep 0.5; done; }
report() { curl -s -X POST http://127.0.0.1:2191$1/cluster/_internal/reclaim/report; }
for i in 0 1 2; do start $i; done; sleep 2
$S/s3venv/bin/python - <<'PY'
import boto3, json
from botocore.config import Config
c=boto3.client("s3",endpoint_url="http://127.0.0.1:21910",aws_access_key_id="AKIAIOSFODNN7EXAMPLE",aws_secret_access_key="wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",region_name="us-east-1",config=Config(s3={"addressing_style":"path"}))
c.create_bucket(Bucket="reclaim")
sizes={f"o{i}": (4096 if i%2==0 else 100_000) for i in range(20)}
for k,n in sizes.items(): c.put_object(Bucket="reclaim",Key=k,Body=bytes([1])*n)
dead=[]  # sizes of blobs that should become unreferenced
for k in ["o0","o1","o2","o3","o4"]: c.put_object(Bucket="reclaim",Key=k,Body=bytes([2])*sizes[k]); dead.append(sizes[k])
for k in ["o5","o6","o7","o8"]: c.delete_object(Bucket="reclaim",Key=k); dead.append(sizes[k])
P=5*1024*1024
u=c.create_multipart_upload(Bucket="reclaim",Key="aborted")["UploadId"]
for n in (1,2): c.upload_part(Bucket="reclaim",Key="aborted",UploadId=u,PartNumber=n,Body=bytes([3])*P)
c.abort_multipart_upload(Bucket="reclaim",Key="aborted",UploadId=u); dead += [P,P]
u=c.create_multipart_upload(Bucket="reclaim",Key="done")["UploadId"]
parts=[{"PartNumber":n,"ETag":c.upload_part(Bucket="reclaim",Key="done",UploadId=u,PartNumber=n,Body=bytes([4])*P)["ETag"]} for n in (1,2)]
c.complete_multipart_upload(Bucket="reclaim",Key="done",UploadId=u,MultipartUpload={"Parts":parts})
shard=lambda n:(n+1)//2  # RS(2,1): each shard is ceil(len/2)
json.dump({"count":len(dead),"bytes":sum(shard(n) for n in dead)},open("/tmp/step1-expected.json","w"))
print("expected per node:", len(dead), "shards,", sum(shard(n) for n in dead), "bytes")
PY
sleep 4
echo "== (a) every node, all records present"; for i in 0 1 2; do echo "node$i $(report $i)"; done
echo "== (b) node2 restarted with its metadata log wiped (missed every row)"
stop 2; rm -f $R/n2/cluster_metadata.log; start 2; sleep 1; echo "node2 $(report 2)"
echo "== peer down: node1 stopped, node0 reports"
stop 1; echo "node0 $(report 0)"; start 1
$S/s3venv/bin/python - <<'PY'
import boto3
from botocore.config import Config
c=boto3.client("s3",endpoint_url="http://127.0.0.1:21910",aws_access_key_id="AKIAIOSFODNN7EXAMPLE",aws_secret_access_key="wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",region_name="us-east-1",config=Config(s3={"addressing_style":"path"}))
ok=all(c.get_object(Bucket="reclaim",Key=f"o{i}")["Body"].read()[:1]==(bytes([2]) if i<5 else bytes([1])) for i in range(20) if not 5<=i<=8)
print("live objects still read back correctly:", ok, "| completed multipart intact:", c.get_object(Bucket="reclaim",Key="done")["Body"].read()==bytes([4])*(10*1024*1024))
PY
for i in 0 1 2; do stop $i; done; grep -h "panicked" $R/*/node.out | head -2; rm -rf $R
