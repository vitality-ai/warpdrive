import boto3, os, time, requests, json
from botocore.client import Config
from botocore.auth import SigV4Auth
from botocore.awsrequest import AWSRequest
from botocore.credentials import Credentials

WARPDRIVE_URL = 'http://10.128.0.4:9710'
ACCESS_KEY = 'adminkey'
SECRET_KEY = 'adminsecretkey123456'
BUCKET = 'slab-batch-test'
OBJ_SIZE = 8 * 1024 * 1024

client = boto3.client('s3', endpoint_url=WARPDRIVE_URL,
                       aws_access_key_id=ACCESS_KEY, aws_secret_access_key=SECRET_KEY,
                       config=Config(signature_version='s3v4', s3={'addressing_style': 'path'}),
                       region_name='us-east-1')
try:
    client.create_bucket(Bucket=BUCKET)
except Exception as e:
    print('create_bucket:', e)

def signed_get(url, params):
    req = AWSRequest(method='GET', url=url, params=params)
    creds = Credentials(ACCESS_KEY, SECRET_KEY)
    SigV4Auth(creds, 's3', 'us-east-1').add_auth(req)
    prepared = req.prepare()
    return requests.get(prepared.url, headers=dict(prepared.headers), stream=True)

K_VALUES = [1, 4, 16, 64, 256]
payload = os.urandom(OBJ_SIZE)
results = []

for k in K_VALUES:
    hint = f'batch-hint-{k}'
    for i in range(k):
        client.put_object(Bucket=BUCKET, Key=f'{hint}-{i}.bin', Body=payload,
                           Metadata={'warpd-slab-hint': hint})

    t0 = time.time()
    r = signed_get(f'{WARPDRIVE_URL}/_warpd/slab/{BUCKET}', {'hint': hint})
    total_bytes = 0
    for chunk in r.iter_content(chunk_size=1 << 20):
        total_bytes += len(chunk)
    elapsed = time.time() - t0
    mb = total_bytes / 1e6
    throughput = mb / elapsed if elapsed > 0 else 0
    count_hdr = r.headers.get('x-warpd-slab-count')
    print(f'k={k:4d}  bytes={total_bytes:12d} ({mb:.1f} MB)  time={elapsed:.4f}s  '
          f'throughput={throughput:.1f} MB/s  status={r.status_code}  count_hdr={count_hdr}')
    results.append({'k': k, 'bytes': total_bytes, 'time_s': elapsed,
                     'throughput_mbps': throughput, 'status': r.status_code,
                     'count_header': count_hdr})

with open('/mnt/batch_get_results.json', 'w') as f:
    json.dump(results, f, indent=2)
print('saved /mnt/batch_get_results.json')
