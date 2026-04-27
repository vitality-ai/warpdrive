# S3-Compatible API

WarpDrive provides full S3 compatibility for seamless integration with existing S3 tools and libraries.

## ✅ **Implemented Features**

- **Core Operations**: PUT, GET, DELETE, HEAD, LIST
- **Advanced Operations**: COPY, Multipart Upload
- **Authentication**: AWS Signature V4
- **Unified Storage**: Same backend as native API

## 🚀 **Quick Start**

**Authentication:** Warpdrive uses Vitality Console only. Create an API key in Console and use that access key + secret for all S3 requests. Set `VITALITY_CONSOLE_URL` and `WARPDRIVE_SERVICE_SECRET` in Warpdrive's `.env`.

### Using boto3 (Python)

```python
import boto3

# Use credentials from Vitality Console (API key)
s3 = boto3.client(
    's3',
    endpoint_url='http://localhost:9710/s3',
    aws_access_key_id='<your Console API access key>',
    aws_secret_access_key='<your Console API secret key>',
    region_name='us-east-1'
)

# Upload
s3.upload_file('local_file.txt', 'my-bucket', 'remote_file.txt')

# Download
s3.download_file('my-bucket', 'remote_file.txt', 'downloaded_file.txt')

# List objects
response = s3.list_objects_v2(Bucket='my-bucket')
```

### Using aws-cli

```bash
export AWS_ACCESS_KEY_ID=<your Console API access key>
export AWS_SECRET_ACCESS_KEY=<your Console API secret key>
export AWS_DEFAULT_REGION=us-east-1

# Upload
aws s3 cp local_file.txt s3://my-bucket/remote_file.txt --endpoint-url http://localhost:9710/s3

# Download
aws s3 cp s3://my-bucket/remote_file.txt downloaded_file.txt --endpoint-url http://localhost:9710/s3

# List
aws s3 ls s3://my-bucket/ --endpoint-url http://localhost:9710/s3
```

## 🧪 **Testing**

Run the comprehensive test:

```bash
cd demo/
python3 s3_comprehensive_test.py
```

**Test Coverage:**
- 13 different file types (videos, images, documents, binary)
- File integrity verification
- All S3 operations (PUT, GET, DELETE, HEAD, COPY, LIST)
- Multipart uploads
- Error handling

## 📚 **Documentation**

- **[User Guide](../docs/user_guide.md#s3-compatible-api)**: Complete API reference
- **[Technical Architecture](../docs/Technical-Architecture.md)**: System design
- **[Demo](../demo/)**: Working examples and test files