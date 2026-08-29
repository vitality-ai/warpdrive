import boto3, os
from botocore.client import Config

client = boto3.client('s3', endpoint_url='http://127.0.0.1:9710',
                       aws_access_key_id='adminkey', aws_secret_access_key='adminsecretkey123456',
                       config=Config(signature_version='s3v4', s3={'addressing_style': 'path'}),
                       region_name='us-east-1')
BUCKET = 'realistic-delta-test'
N_TOTAL = 8
OBJ_SIZE = 256 * 1024 * 1024

payload = os.urandom(OBJ_SIZE)

for nworkers in [2, 4, 8]:
    per_group = N_TOTAL // nworkers
    for w in range(nworkers):
        hint = f'group-{nworkers}-{w}'
        for i in range(per_group):
            key = f'{hint}-obj-{i}.bin'
            client.put_object(Bucket=BUCKET, Key=key, Body=payload, Metadata={'warpd-slab-hint': hint})
    print(f'nworkers={nworkers}: {nworkers} groups x {per_group} objects x 256MiB uploaded')
