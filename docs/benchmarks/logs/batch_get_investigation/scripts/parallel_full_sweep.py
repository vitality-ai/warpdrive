import time, requests, statistics
from concurrent.futures import ThreadPoolExecutor
from botocore.auth import SigV4Auth
from botocore.awsrequest import AWSRequest
from botocore.credentials import Credentials

WARPDRIVE_URL = 'http://10.128.0.4:9710'
BUCKET = 'parallel-batch-test'

def signed_get(url, params):
    req = AWSRequest(method='GET', url=url, params=params)
    creds = Credentials('adminkey', 'adminsecretkey123456')
    SigV4Auth(creds, 's3', 'us-east-1').add_auth(req)
    prepared = req.prepare()
    return requests.get(prepared.url, headers=dict(prepared.headers), stream=True)

def fetch_hint(hint):
    r = signed_get(f'{WARPDRIVE_URL}/_warpd/slab/{BUCKET}', {'hint': hint})
    total = 0
    for chunk in r.iter_content(chunk_size=1 << 20):
        total += len(chunk)
    return total

def run(nhints):
    hints = [f'pf{nhints}-{h}' for h in range(nhints)]
    t0 = time.time()
    with ThreadPoolExecutor(max_workers=nhints) as ex:
        results = list(ex.map(fetch_hint, hints))
    elapsed = time.time() - t0
    total = sum(results)
    return (total / 1e6) / elapsed

for nhints in [2, 4, 8, 16, 32, 64]:
    run(nhints)  # warmup
    samples = [run(nhints) for _ in range(10)]
    med = statistics.median(samples)
    lo, hi = min(samples), max(samples)
    stdev = statistics.stdev(samples)
    print(f'nhints={nhints:3d}  median={med:7.1f} MB/s  range=[{lo:.1f}, {hi:.1f}]  stdev={stdev:.1f}  samples={[round(s) for s in samples]}')
