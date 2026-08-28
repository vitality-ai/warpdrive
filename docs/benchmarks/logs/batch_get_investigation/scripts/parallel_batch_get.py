import time, requests
from concurrent.futures import ThreadPoolExecutor
from botocore.auth import SigV4Auth
from botocore.awsrequest import AWSRequest
from botocore.credentials import Credentials

WARPDRIVE_URL = 'http://10.128.0.4:9710'
BUCKET = 'parallel-batch-test'
NHINTS = 8

def signed_get(url, params):
    req = AWSRequest(method='GET', url=url, params=params)
    creds = Credentials('adminkey', 'adminsecretkey123456')
    SigV4Auth(creds, 's3', 'us-east-1').add_auth(req)
    prepared = req.prepare()
    return requests.get(prepared.url, headers=dict(prepared.headers), stream=True)

def fetch_hint(h):
    hint = f'pgroup-{h}'
    r = signed_get(f'{WARPDRIVE_URL}/_warpd/slab/{BUCKET}', {'hint': hint})
    total = 0
    for chunk in r.iter_content(chunk_size=1 << 20):
        total += len(chunk)
    return total

def run(nthreads):
    t0 = time.time()
    with ThreadPoolExecutor(max_workers=nthreads) as ex:
        results = list(ex.map(fetch_hint, range(NHINTS)))
    elapsed = time.time() - t0
    total = sum(results)
    mb = total / 1e6
    print(f'threads={nthreads} bytes={total} time={elapsed:.4f}s throughput={mb/elapsed:.1f} MB/s')

# warmup
run(NHINTS)
print('--- timed runs ---')
for i in range(5):
    run(NHINTS)
