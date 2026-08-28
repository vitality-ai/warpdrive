import time, requests
from botocore.auth import SigV4Auth
from botocore.awsrequest import AWSRequest
from botocore.credentials import Credentials

WARPDRIVE_URL = 'http://10.128.0.4:9710'
BUCKET = 'slab-batch-test'
HINT = 'batch-hint-256'

def signed_get(url, params):
    req = AWSRequest(method='GET', url=url, params=params)
    creds = Credentials('adminkey', 'adminsecretkey123456')
    SigV4Auth(creds, 's3', 'us-east-1').add_auth(req)
    prepared = req.prepare()
    return requests.get(prepared.url, headers=dict(prepared.headers), stream=True)

def run():
    t0 = time.time()
    r = signed_get(f'{WARPDRIVE_URL}/_warpd/slab/{BUCKET}', {'hint': HINT})
    total = 0
    for chunk in r.iter_content(chunk_size=1 << 20):
        total += len(chunk)
    elapsed = time.time() - t0
    mb = total / 1e6
    print(f'bytes={total} time={elapsed:.4f}s throughput={mb/elapsed:.1f} MB/s status={r.status_code}')

run()
