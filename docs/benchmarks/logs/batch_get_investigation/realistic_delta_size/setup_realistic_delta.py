import boto3, os
from botocore.client import Config

client = boto3.client('s3', endpoint_url='http://127.0.0.1:9710',
                       aws_access_key_id='adminkey', aws_secret_access_key='adminsecretkey123456',
                       config=Config(signature_version='s3v4', s3={'addressing_style': 'path'}),
                       region_name='us-east-1')
BUCKET = 'realistic-delta-test'
try:
    client.create_bucket(Bucket=BUCKET)
except Exception as e:
    print('create_bucket:', e)

N_OBJECTS = 8
OBJ_SIZE = 256 * 1024 * 1024  # 256 MiB, matches Neon's DEFAULT_CHECKPOINT_DISTANCE
HINT = 'checkpoint-epoch-1'

print(f'generating {OBJ_SIZE/1e6:.0f} MB payload...')
payload = os.urandom(OBJ_SIZE)
for i in range(1, N_OBJECTS + 1):
    key = f'layer-{i}.bin'  # 1-indexed, matches AnyBlob's {filePath}{1..n}.bin convention
    client.put_object(Bucket=BUCKET, Key=key, Body=payload, Metadata={'warpd-slab-hint': HINT})
    print(f'uploaded {key}')

print(f'total: {N_OBJECTS} objects x {OBJ_SIZE/1e6:.0f} MB = {N_OBJECTS*OBJ_SIZE/1e9:.2f} GB')
