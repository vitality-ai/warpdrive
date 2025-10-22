# S3-Compatible API

WarpDrive provides full S3 compatibility for seamless integration with existing S3 tools and libraries.

## ✅ **Implemented Features**

- **Core Operations**: PUT, GET, DELETE, HEAD, LIST
- **Advanced Operations**: COPY, Multipart Upload
- **Authentication**: AWS Signature V4
- **Unified Storage**: Same backend as native API

## 🚀 **Quick Start**

### Using boto3 (Python)

```python
import boto3

s3 = boto3.client(
    's3',
    endpoint_url='http://localhost:9710',
    aws_access_key_id='AKIAIOSFODNN7EXAMPLE',
    aws_secret_access_key='wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY',
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
export AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE
export AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY
export AWS_DEFAULT_REGION=us-east-1

# Upload
aws s3 cp local_file.txt s3://my-bucket/remote_file.txt --endpoint-url http://localhost:9710

# Download
aws s3 cp s3://my-bucket/remote_file.txt downloaded_file.txt --endpoint-url http://localhost:9710

# List
aws s3 ls s3://my-bucket/ --endpoint-url http://localhost:9710
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