import time, requests, statistics, subprocess, re
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

def run_batch_get():
    t0 = time.time()
    r = signed_get(f'{WARPDRIVE_URL}/_warpd/slab/{BUCKET}', {'hint': HINT})
    total = 0
    for chunk in r.iter_content(chunk_size=1 << 20):
        total += len(chunk)
    elapsed = time.time() - t0
    return (total / 1e6) / elapsed

def run_sequential():
    cmd = [
        './AnyBlobBenchmark', 'minio', 'bandwidth',
        '-w', '10.128.0.4', '-p', '9710', '-b', 'anyblob-bench', '-r', 'us-east-1',
        '-e', 'adminkey', '-k', '/home/nash/minio_secret.txt',
        '-n', '100', '-l', '256', '-c', '1', '-t', '1', '-s', '65536',
    ]
    out = subprocess.run(cmd, cwd='/home/nash/cj/warpdrive/AnyBlob/example/benchmark/build',
                          capture_output=True, text=True).stdout
    line = out.strip().splitlines()[-1]
    fields = line.split(',')
    time_ms = float(fields[2])
    datasize = int(fields[14])
    return (datasize / 1e6) / (time_ms / 1000)

print('=== batch-GET, 256 objects (2GB), 1 connection ===')
run_batch_get()  # warmup
samples = [run_batch_get() for _ in range(10)]
med = statistics.median(samples)
print(f'median={med:.1f} MB/s range=[{min(samples):.1f}, {max(samples):.1f}] stdev={statistics.stdev(samples):.1f}')
print('samples=', [round(s) for s in samples])

print()
print('=== sequential 256x8MiB individual GETs, 1 connection ===')
run_sequential()  # warmup
samples2 = [run_sequential() for _ in range(10)]
med2 = statistics.median(samples2)
print(f'median={med2:.1f} MB/s range=[{min(samples2):.1f}, {max(samples2):.1f}] stdev={statistics.stdev(samples2):.1f}')
print('samples=', [round(s) for s in samples2])
