import boto3
from botocore.client import Config
c = boto3.client('s3', endpoint_url='http://127.0.0.1:9710',
                  aws_access_key_id='adminkey', aws_secret_access_key='adminsecretkey123456',
                  config=Config(signature_version='s3v4', s3={'addressing_style': 'path'}),
                  region_name='us-east-1')
paginator = c.get_paginator('list_objects_v2')
to_delete = []
for page in paginator.paginate(Bucket='parallel-batch-test'):
    for obj in page.get('Contents', []):
        to_delete.append({'Key': obj['Key']})
for i in range(0, len(to_delete), 1000):
    c.delete_objects(Bucket='parallel-batch-test', Delete={'Objects': to_delete[i:i+1000]})
print('deleted', len(to_delete))
