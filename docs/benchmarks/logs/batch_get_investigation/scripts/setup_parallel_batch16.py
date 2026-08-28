import boto3, os
from botocore.client import Config

client = boto3.client('s3', endpoint_url='http://127.0.0.1:9710',
                       aws_access_key_id='adminkey', aws_secret_access_key='adminsecretkey123456',
                       config=Config(signature_version='s3v4', s3={'addressing_style': 'path'}),
                       region_name='us-east-1')
BUCKET = 'parallel-batch-test'
NHINTS = 16
OBJS_PER_HINT = 16
OBJ_SIZE = 8 * 1024 * 1024
payload = os.urandom(OBJ_SIZE)

for h in range(NHINTS):
    hint = f'p16group-{h}'
    for i in range(OBJS_PER_HINT):
        client.put_object(Bucket=BUCKET, Key=f'{hint}-{i}.bin', Body=payload,
                           Metadata={'warpd-slab-hint': hint})
print(f'total: {NHINTS} hints x {OBJS_PER_HINT} objects x 8MB = {NHINTS*OBJS_PER_HINT*OBJ_SIZE/1e9:.2f} GB')
