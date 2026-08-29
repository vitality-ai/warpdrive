import time, requests, statistics
from concurrent.futures import ThreadPoolExecutor
from botocore.auth import SigV4Auth
from botocore.awsrequest import AWSRequest
from botocore.credentials import Credentials

WARPDRIVE_URL = 'http://10.128.0.4:9710'
BUCKET = 'anyblob-bench'  # 100 objects, 1.bin..100.bin, 8 MiB each -- reuse existing corpus
TOTAL_OBJS = 256

def signed_get(url):
    req = AWSRequest(method='GET', url=url)
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

def fetch_share(keys):
    # one worker's share: sequential individual GETs, one connection reused
    total = 0
    for k in keys:
        total += fetch_one(k)
    return total

def run(nworkers):
    # 256 objects total, cycling through the 100-object corpus, split evenly
    all_keys = [f'{(i % 100) + 1}.bin' for i in range(TOTAL_OBJS)]
    shares = [all_keys[i::nworkers] for i in range(nworkers)]
    t0 = time.time()
    with ThreadPoolExecutor(max_workers=nworkers) as ex:
        results = list(ex.map(fetch_share, shares))
    elapsed = time.time() - t0
    total = sum(results)
    return (total / 1e6) / elapsed

for nworkers in [2, 4, 8, 16, 32, 64]:
    run(nworkers)  # warmup
    samples = [run(nworkers) for _ in range(10)]
    med = statistics.median(samples)
    print(f'nworkers={nworkers:3d}  median={med:7.1f} MB/s  range=[{min(samples):.1f}, {max(samples):.1f}]  '
          f'stdev={statistics.stdev(samples):.1f}  samples={[round(s) for s in samples]}')
