import time, requests, statistics
from botocore.auth import SigV4Auth
from botocore.awsrequest import AWSRequest
from botocore.credentials import Credentials

WARPDRIVE_URL = 'http://10.128.0.4:9710'
BUCKET = 'realistic-delta-test'
HINT = 'checkpoint-epoch-1'
N_OBJECTS = 8

def signed_get(url, params=None):
    req = AWSRequest(method='GET', url=url, params=params or {})
    creds = Credentials('adminkey', 'adminsecretkey123456')
    SigV4Auth(creds, 's3', 'us-east-1').add_auth(req)
    prepared = req.prepare()
    return requests.get(prepared.url, headers=dict(prepared.headers), stream=True)

def fetch_one(key):
    r = signed_get(f'{WARPDRIVE_URL}/{BUCKET}/{key}')
    total = 0
    for chunk in r.iter_content(chunk_size=1 << 20):
        total += len(chunk)
    return total

def run_batch_single():
    t0 = time.time()
    r = signed_get(f'{WARPDRIVE_URL}/_warpd/slab/{BUCKET}', {'hint': HINT})
    total = 0
    for chunk in r.iter_content(chunk_size=1 << 20):
        total += len(chunk)
    elapsed = time.time() - t0
    return (total / 1e6) / elapsed

def run_sequential_single():
    t0 = time.time()
    total = 0
    for i in range(1, N_OBJECTS + 1):
        total += fetch_one(f'layer-{i}.bin')
    elapsed = time.time() - t0
    return (total / 1e6) / elapsed

def run_stats(fn, n=10):
    fn()
    samples = [fn() for _ in range(n)]
    return {
        'median': statistics.median(samples),
        'min': min(samples),
        'max': max(samples),
        'stdev': statistics.stdev(samples),
        'samples': [round(s, 1) for s in samples],
    }

print('=== single connection: batch-GET (8 layers, 1 request) ===')
r = run_stats(run_batch_single)
print(r)

print('=== single connection: sequential individual GETs (8 layers, 8 requests) ===')
r = run_stats(run_sequential_single)
print(r)
