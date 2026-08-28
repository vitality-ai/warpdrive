import boto3, os
from botocore.client import Config

client = boto3.client('s3', endpoint_url='http://127.0.0.1:9710',
                       aws_access_key_id='adminkey', aws_secret_access_key='adminsecretkey123456',
                       config=Config(signature_version='s3v4', s3={'addressing_style': 'path'}),
                       region_name='us-east-1')
BUCKET = 'parallel-batch-test'
try:
    client.create_bucket(Bucket=BUCKET)
except Exception:
    pass

OBJ_SIZE = 8 * 1024 * 1024
payload = os.urandom(OBJ_SIZE)
TOTAL_OBJS = 256  # keep total data volume constant at 2GB across all configs

for nhints in [2, 4, 8, 16, 32, 64]:
    objs_per_hint = TOTAL_OBJS // nhints
    for h in range(nhints):
        hint = f'pf{nhints}-{h}'
        for i in range(objs_per_hint):
            client.put_object(Bucket=BUCKET, Key=f'{hint}-{i}.bin', Body=payload,
                               Metadata={'warpd-slab-hint': hint})
    print(f'nhints={nhints} objs_per_hint={objs_per_hint} total={nhints*objs_per_hint}')
