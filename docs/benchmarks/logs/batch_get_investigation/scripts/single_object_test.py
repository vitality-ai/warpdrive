import boto3, os, time
from botocore.client import Config

WARPDRIVE_URL = 'http://10.128.0.4:9710'
ACCESS_KEY = 'adminkey'
SECRET_KEY = 'adminsecretkey123456'
BUCKET = 'single-obj-test'
SIZE = 2147483648  # 2 GiB, matching the k=256 batch total

client = boto3.client('s3', endpoint_url=WARPDRIVE_URL,
                       aws_access_key_id=ACCESS_KEY, aws_secret_access_key=SECRET_KEY,
                       config=Config(signature_version='s3v4', s3={'addressing_style': 'path'}),
                       region_name='us-east-1')
try:
    client.create_bucket(Bucket=BUCKET)
except Exception as e:
    print('create_bucket:', e)

print('generating payload...')
payload = os.urandom(SIZE)
print('uploading...')
t0 = time.time()
client.put_object(Bucket=BUCKET, Key='big.bin', Body=payload)
put_elapsed = time.time() - t0
print(f'PUT: {SIZE/1e6:.1f} MB in {put_elapsed:.3f}s = {SIZE/1e6/put_elapsed:.1f} MB/s')
del payload

print('downloading...')
t0 = time.time()
obj = client.get_object(Bucket=BUCKET, Key='big.bin')
total = 0
for chunk in obj['Body'].iter_chunks(chunk_size=1 << 20):
    total += len(chunk)
get_elapsed = time.time() - t0
print(f'GET: {total/1e6:.1f} MB in {get_elapsed:.3f}s = {total/1e6/get_elapsed:.1f} MB/s')
