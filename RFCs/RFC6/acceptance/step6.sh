#!/usr/bin/env bash
# RFC 6 step 6 acceptance: gc/ rows free what overwrites, deletes, re-uploaded parts and aborts kill,
# with no sweep at all. A writer and readers run throughout. The audit sweep must then find nothing left.
# and every live object still reads back exactly, during compaction, after it, and after a restart.
S=${S:?set S to a scratch dir holding wt-rfc6, s3venv and server_log.yaml}
BIN=$S/wt-rfc6/server/target/release/warp_drive; R=$S/step6-cluster; rm -rf $R
# Layout: N nodes, RS(K,M). Default RS(3,2) on 5 nodes, the layout deployments use.
export N=${N:-5} K=${K:-3} M=${M:-2}
PEERS=$(for i in $(seq 0 $((N-1))); do printf "http://127.0.0.1:2191%d," $i; done); PEERS=${PEERS%,}
ENVS="WARPDRIVE_SEGMENT_MB=1 WARPDRIVE_RECLAIM_GRACE_SECS=3 WARPDRIVE_RECLAIM_INTERVAL_SECS=999999 WARPDRIVE_RECLAIM_CONFIRM_SECS=2 WARPDRIVE_COMPACT_SEAL_SECS=2 WARPDRIVE_COMPACT_UNLINK_SECS=1 WARPDRIVE_COMPACT_MBPS=200"
start() { i=$1; mkdir -p $R/n$i; cp $S/server_log.yaml $R/n$i/
  (cd $R/n$i && env $ENVS WARPDRIVE_PORT=2191$i WARPDRIVE_SELF_ADDR=http://127.0.0.1:2191$i WARPDRIVE_PEERS=$PEERS WARPDRIVE_RS_K=$K WARPDRIVE_RS_M=$M WARPDRIVE_ADMIN_ACCESS_KEY=AKIAIOSFODNN7EXAMPLE WARPDRIVE_ADMIN_SECRET_KEY=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY nohup $BIN >> node.out 2>&1 &)
  for k in $(seq 1 30); do curl -s -o /dev/null http://127.0.0.1:2191$i/ && break; sleep 0.5; done; }
stop() { i=$1; for d in 0 1000 2000; do lsof -ti tcp:$((21910+i+d)) -sTCP:LISTEN | xargs kill 2>/dev/null; done
  for k in $(seq 1 30); do busy=0; for d in 0 1000 2000; do lsof -ti tcp:$((21910+i+d)) -sTCP:LISTEN >/dev/null && busy=1; done; [ $busy = 0 ] && break; sleep 0.5; done; }
for i in $(seq 0 $((N-1))); do start $i; done; sleep 2
R=$R $S/s3venv/bin/python - <<'PY'
import boto3, json, os, random, threading, time, urllib.request
from botocore.config import Config
R=os.environ["R"]; NN=int(os.environ["N"])
c=boto3.client("s3",endpoint_url="http://127.0.0.1:21910",aws_access_key_id="AKIAIOSFODNN7EXAMPLE",aws_secret_access_key="wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",region_name="us-east-1",config=Config(s3={"addressing_style":"path"},max_pool_connections=32))
def disk():
    out=[]
    for i in range(NN):
        n=0
        for root,_,files in os.walk(f"{R}/n{i}/storage"):
            for f in files:
                if f.endswith(".seg"): n+=os.stat(os.path.join(root,f)).st_blocks*512
        out.append(n)
    return out
def post(i,path): return json.loads(urllib.request.urlopen(urllib.request.Request(f"http://127.0.0.1:2191{i}{path}",method="POST"),timeout=600).read())
body=lambda k,g,n: (f"{k}-{g}-".encode()*(n//8+1))[:n]
random.seed(11); want={}
c.create_bucket(Bucket="gcb")
sizes={f"o{i}": random.choice([4096, 20_000, 100_000, 300_000]) for i in range(300)}
for k,n in sizes.items(): c.put_object(Bucket="gcb",Key=k,Body=body(k,0,n)); want[("gcb",k,None)]=body(k,0,n)
keys=list(sizes)
for k in keys[:150]: c.put_object(Bucket="gcb",Key=k,Body=body(k,1,sizes[k])); want[("gcb",k,None)]=body(k,1,sizes[k])
for k in keys[150:210]: c.delete_object(Bucket="gcb",Key=k); del want[("gcb",k,None)]
# versions: delete an old version by id
c.create_bucket(Bucket="gcv"); c.put_bucket_versioning(Bucket="gcv",VersioningConfiguration={"Status":"Enabled"})
v1=c.put_object(Bucket="gcv",Key="x",Body=body("x",1,200_000))["VersionId"]; v2=c.put_object(Bucket="gcv",Key="x",Body=body("x",2,200_000))["VersionId"]
c.delete_object(Bucket="gcv",Key="x",VersionId=v1); want[("gcv","x",v2)]=body("x",2,200_000)
# multipart: part 2 uploaded twice, part 3 left out of the completion; one upload aborted
P=5*1024*1024
u=c.create_multipart_upload(Bucket="gcb",Key="mp")["UploadId"]
e1=c.upload_part(Bucket="gcb",Key="mp",UploadId=u,PartNumber=1,Body=bytes([1])*P)["ETag"]
c.upload_part(Bucket="gcb",Key="mp",UploadId=u,PartNumber=2,Body=bytes([9])*P)
e2=c.upload_part(Bucket="gcb",Key="mp",UploadId=u,PartNumber=2,Body=bytes([2])*P)["ETag"]
c.upload_part(Bucket="gcb",Key="mp",UploadId=u,PartNumber=3,Body=bytes([3])*P)
c.complete_multipart_upload(Bucket="gcb",Key="mp",UploadId=u,MultipartUpload={"Parts":[{"PartNumber":1,"ETag":e1},{"PartNumber":2,"ETag":e2}]})
want[("gcb","mp",None)]=bytes([1])*P+bytes([2])*P
a=c.create_multipart_upload(Bucket="gcb",Key="ab")["UploadId"]
for n in (1,2): c.upload_part(Bucket="gcb",Key="ab",UploadId=a,PartNumber=n,Body=bytes([7])*P)
c.abort_multipart_upload(Bucket="gcb",Key="ab",UploadId=a)
before=disk(); live=sum(len(v) for v in want.values())
print(f"live bytes {live} (about {live//int(os.environ['K'])} per node); before: {[round(b/1e6,1) for b in before]} MB")
time.sleep(4)  # past the grace period
stop=False; errs=[]; extra={}
def writer():
    i=0
    while not stop:
        k=f"during{i}"; b=body(k,0,30_000)
        try: c.put_object(Bucket="gcb",Key=k,Body=b); extra[("gcb",k,None)]=b
        except Exception as e: errs.append(f"put {type(e).__name__}")
        i+=1
def get(b,k,v): return c.get_object(Bucket=b,Key=k,**({"VersionId":v} if v else {}))["Body"].read()
def reader():
    ks=list(want)
    while not stop:
        t=random.choice(ks)
        try:
            if get(*t)!=want[t]: errs.append(f"get {t}: wrong bytes")
        except Exception as e: errs.append(f"get {t} {type(e).__name__}")
ts=[threading.Thread(target=writer)]+[threading.Thread(target=reader) for _ in range(4)]; [t.start() for t in ts]
first=[post(i,"/cluster/_internal/gc/run") for i in range(NN)]
for i,g in enumerate(first): print(f"gc node{i}: rows {g['rows']}, blobs {g['blobs']}, shards {g['shards']}, dropped {g['compact']['dead_entries_dropped']}, compacted {g['compact']['segments_compacted']}, punched {g['compact']['bytes_punched']}")
time.sleep(2); [post(i,"/cluster/_internal/gc/run") for i in range(NN)]
stop=True; [t.join() for t in ts]
after=disk()
print("after:", [round(b/1e6,1) for b in after], "MB; freed", [f"{100*(b0-b1)//b0}%" for b0,b1 in zip(before,after)])
print("errors while freeing:", len(errs), errs[:3])
again=[post(i,"/cluster/_internal/gc/run")["rows"] for i in range(NN)]; print("rows processed again on a third pass:", again)
want.update(extra)
bad=[t for t,v in want.items() if get(*t)!=v]
print(f"after: {len(want)-len(bad)}/{len(want)} objects read back exactly", bad[:3])
time.sleep(4)
audit=[post(i,"/cluster/_internal/reclaim/report") for i in range(NN)]
print("audit sweep, reclaimable left per node:", [a["reclaimable_superseded"]+a["reclaimable_confirmed"] for a in audit], "(the mp completion's left-out part 3 counts if missed)")
json.dump([[list(t),v.hex()] for t,v in want.items()], open(f"{R}/want.json","w"))
PY
for i in $(seq 0 $((N-1))); do stop $i; done; for i in $(seq 0 $((N-1))); do start $i; done; sleep 2
R=$R $S/s3venv/bin/python - <<'PY'
import boto3, json, os
from botocore.config import Config
c=boto3.client("s3",endpoint_url="http://127.0.0.1:21910",aws_access_key_id="AKIAIOSFODNN7EXAMPLE",aws_secret_access_key="wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",region_name="us-east-1",config=Config(s3={"addressing_style":"path"}))
want=[(tuple(t),bytes.fromhex(v)) for t,v in json.load(open(os.environ["R"]+"/want.json"))]
get=lambda b,k,v: c.get_object(Bucket=b,Key=k,**({"VersionId":v} if v else {}))["Body"].read()
bad=[t for t,v in want if get(*t)!=v]
print(f"after restart: {len(want)-len(bad)}/{len(want)} objects read back exactly", bad[:3])
PY
for i in $(seq 0 $((N-1))); do stop $i; done; grep -rh "panicked\|gc: saving\|still has" $R | head -3
