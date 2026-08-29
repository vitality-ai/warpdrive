import time, requests, statistics
from concurrent.futures import ThreadPoolExecutor
from botocore.auth import SigV4Auth
from botocore.awsrequest import AWSRequest
from botocore.credentials import Credentials

WARPDRIVE_URL = 'http://10.128.0.4:9710'
BUCKET = 'realistic-delta-test'
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

def fetch_batch_hint(hint):
    r = signed_get(f'{WARPDRIVE_URL}/_warpd/slab/{BUCKET}', {'hint': hint})
    total = 0
    for chunk in r.iter_content(chunk_size=1 << 20):
        total += len(chunk)
    return total

def run_batch_parallel(nworkers):
    # nworkers groups, pre-tagged at setup time as group-{w}-of-{nworkers}
    hints = [f'group-{nworkers}-{w}' for w in range(nworkers)]
    t0 = time.time()
    with ThreadPoolExecutor(max_workers=nworkers) as ex:
        results = list(ex.map(fetch_batch_hint, hints))
    elapsed = time.time() - t0
    return (sum(results) / 1e6) / elapsed

def run_stats(fn, arg, n=10):
    fn(arg)
    samples = [fn(arg) for _ in range(n)]
    return statistics.median(samples), min(samples), max(samples), statistics.stdev(samples), [round(s,1) for s in samples]

for nworkers in [2, 4, 8]:
    med, lo, hi, sd, samples = run_stats(run_batch_parallel, nworkers)
    print(f'batch nworkers={nworkers}: median={med:.1f} range=[{lo:.1f},{hi:.1f}] stdev={sd:.1f} samples={samples}')
