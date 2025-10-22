# User Guide

> **Note:**  
> These instructions assume you have installed and are running the Storage Service locally.  
> For setup instructions, see the [Developer's Documentation](../docs/setup.md).  
> Once our cloud offering is available, we will update this guide with details for connecting to the managed service. However if you are a developer we suggest you follow our [Developer's Documentation](../docs/setup.md) which is self contained to get you started.  

## Getting Started

Install the CIAOS Python client:
```bash
pip install ciaos
```

### Configuration

To use CIAOS, initialize a `Config` object with your details:

- **user_id**: Your user ID.
- **api_url**: The storage server URL.
- **user_access_key**: Your user access key.

Example:
```python
from ciaos import Ciaos, Config

config = Config(
    user_id="your_user_id",
    api_url="https://api.ciaos.com",
    user_access_key="xxxx"
)

ciaos_client = Ciaos(config)
```

---

## API Overview

CIAOS provides two interfaces for storing, retrieving, and managing binary data and files:

### 1. Native CIAOS API
Direct interface for storing, retrieving, and managing binary data and files using unique keys.

**Main Methods:**
- `put`: Upload a file to the server.
- `put_binary`: Upload binary data with a unique key.
- `get`: Retrieve binary data by key.
- `update`: Replace the content of an existing key.
- `update_key`: Rename the identifier (key) of existing data.
- `delete`: Remove data by key.
- `append`: Add binary data to an existing key.

### 2. S3-Compatible API
Full S3 compatibility for seamless integration with existing S3 tools and libraries.

**Supported S3 Operations:**
- `PUT`: Upload objects to buckets
- `GET`: Download objects from buckets  
- `DELETE`: Remove objects from buckets
- `HEAD`: Get object metadata
- `COPY`: Copy objects within or between buckets
- `LIST`: List objects in buckets
- `Multipart Upload`: Handle large file uploads in parts

**Authentication:** Uses AWS Signature V4 for secure access control.

---

## Usage Examples

### Native CIAOS API

```python
# PUT: Upload a file. Uses the filename as the key if no key is provided.
ciaos_client.put(file_path="path/to/your/file.txt", key="optional_unique_key")

# PUT_BINARY: Upload binary data with a key.
ciaos_client.put_binary(key="unique_key", data_list=[b"file1_binary", b"file2_binary_data"])

# GET: Retrieve data by key.
data = ciaos_client.get(key="your_key")

# UPDATE: Replace the content at the given key.
ciaos_client.update(key="your_key", data_list=[b"file1_updated_data", b"file2_updated_data"])

# UPDATE_KEY: Change the identifier (key) for existing data.
ciaos_client.update_key(old_key="old_key", new_key="new_key")

# DELETE: Remove data by key.
ciaos_client.delete(key="your_key")

# APPEND: Add data to an existing key.
ciaos_client.append(key="your_key", data_list=[b"additional_data"])
```

### S3-Compatible API

```python
import boto3

# Configure S3 client to use CIAOS
s3_client = boto3.client(
    's3',
    endpoint_url='http://localhost:9710/s3',
    aws_access_key_id='AKIAIOSFODNN7EXAMPLE',
    aws_secret_access_key='wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY',
    region_name='us-east-1'
)

# Upload a file
s3_client.upload_file('local_file.txt', 'my-bucket', 'remote_file.txt')

# Download a file
s3_client.download_file('my-bucket', 'remote_file.txt', 'downloaded_file.txt')

# List objects
response = s3_client.list_objects_v2(Bucket='my-bucket')
for obj in response.get('Contents', []):
    print(f"Key: {obj['Key']}, Size: {obj['Size']}")

# Delete an object
s3_client.delete_object(Bucket='my-bucket', Key='remote_file.txt')
```

---

## Demo and Examples

Ready to try CIAOS? Check out our comprehensive demos:

### 🚀 **Quick Start Demo**
- **Location**: [`demo/`](../demo/)
- **Features**: Complete S3 compatibility test with 13 different file types
- **Run**: `python3 s3_comprehensive_test.py`

### 📁 **Test Files**
- **Location**: [`demo/test_files/`](../demo/test_files/)
- **Contents**: Sample videos, images, documents, and binary files for testing

### 🐍 **Python Client Demo**
- **Location**: [`demo/pythonTestClient.py`](../demo/pythonTestClient.py)
- **Features**: Native CIAOS API examples

### 🔧 **Development Setup**
For advanced usage and development details, see the [Developer's Documentation](../docs/setup.md).
