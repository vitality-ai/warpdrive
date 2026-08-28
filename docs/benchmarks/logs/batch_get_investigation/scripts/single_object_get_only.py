import boto3, time
from botocore.client import Config

client = boto3.client('s3', endpoint_url='http://10.128.0.4:9710',
                       aws_access_key_id='adminkey', aws_secret_access_key='adminsecretkey123456',
                       config=Config(signature_version='s3v4', s3={'addressing_style': 'path'}),
                       region_name='us-east-1')
print('downloading...')
t0 = time.time()
obj = client.get_object(Bucket='single-obj-test', Key='big.bin')
total = 0
for chunk in obj['Body'].iter_chunks(chunk_size=1 << 20):
    total += len(chunk)
elapsed = time.time() - t0
print(f'GET: {total/1e6:.1f} MB in {elapsed:.3f}s = {total/1e6/elapsed:.1f} MB/s')
