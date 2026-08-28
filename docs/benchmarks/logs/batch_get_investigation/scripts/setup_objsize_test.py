import boto3, os
from botocore.client import Config

client = boto3.client('s3', endpoint_url='http://127.0.0.1:9710',
                       aws_access_key_id='adminkey', aws_secret_access_key='adminsecretkey123456',
                       config=Config(signature_version='s3v4', s3={'addressing_style': 'path'}),
                       region_name='us-east-1')

sizes_mb = [8, 32, 128, 512]
for mb in sizes_mb:
    bucket = f'objsize-{mb}mb'
    try:
        client.create_bucket(Bucket=bucket)
    except Exception:
        pass
    payload = os.urandom(mb * 1024 * 1024)
    client.put_object(Bucket=bucket, Key='1.bin', Body=payload)
    print(f'{bucket}: uploaded 1.bin ({mb} MiB)')
