# User Guide

> **Note:**  
> These instructions assume you have installed and are running Warpdrive locally.  
> For setup instructions, see the [Developer's Documentation](../docs/setup.md).  
> Once our cloud offering is available, we will update this guide with details for connecting to the managed service. However if you are a developer we suggest you follow our [Developer's Documentation](../docs/setup.md) which is self contained to get you started.  

## Getting Started

Warpdrive is S3-compatible — any client or library that speaks S3 follows the same semantics. The example below uses **boto3**.

```bash
pip install boto3
```

### Configuration

```python
import boto3

s3 = boto3.client(
    's3',
    endpoint_url='http://localhost:9710',
    aws_access_key_id='adminkey',
    aws_secret_access_key='adminsecretkey123456',
    region_name='us-east-1',
    config=boto3.session.Config(s3={'addressing_style': 'path'}),
)
```

Two things to note when running locally:
- **Path-style addressing** (`addressing_style: 'path'`) — required because virtual-hosted style (`bucket.localhost`) does not resolve on a local machine.
- **Plain HTTP** (`http://`) — TLS is not configured for local development; use `https://` only when connecting to a hosted instance.

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
