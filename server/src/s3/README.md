# S3-Compatible API Implementation

This module provides S3-compatible API endpoints for the CIAOS system, allowing S3 clients to interact with the storage system using familiar S3 protocols.

## Features

- **S3-Compatible Endpoints**: PUT, GET, DELETE, HEAD, and LIST operations
- **Authentication**: AWS4-HMAC-SHA256 signature support (simplified for testing)
- **Bucket Support**: Multi-tenant bucket isolation
- **Error Handling**: Proper HTTP status codes and error responses

## API Endpoints

### Object Operations

- `PUT /s3/{bucket}/{key}` - Upload an object
- `GET /s3/{bucket}/{key}` - Download an object  
- `DELETE /s3/{bucket}/{key}` - Delete an object
- `HEAD /s3/{bucket}/{key}` - Get object metadata

### Bucket Operations

- `GET /s3/{bucket}?list-type=2` - List objects in a bucket

## Authentication

The S3 API uses AWS4-HMAC-SHA256 authentication. For testing purposes, the following credentials are hardcoded:

- **Access Key**: `AKIAIOSFODNN7EXAMPLE`
- **Secret Key**: `wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY`

### Authorization Header Format

```
Authorization: AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20231201/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-date, Signature=signature
```

## Usage Examples

### Using curl

```bash
# PUT an object
curl -X PUT \
  -H "Authorization: AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20231201/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-date, Signature=signature" \
  -H "Content-Type: application/octet-stream" \
  --data "Hello, World!" \
  http://localhost:9710/s3/my-bucket/my-object

# GET an object
curl -X GET \
  -H "Authorization: AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20231201/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-date, Signature=signature" \
  http://localhost:9710/s3/my-bucket/my-object

# DELETE an object
curl -X DELETE \
  -H "Authorization: AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20231201/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-date, Signature=signature" \
  http://localhost:9710/s3/my-bucket/my-object
```

### Using Python

```python
import requests

# Test S3 API
url = "http://localhost:9710/s3/my-bucket/my-object"
headers = {
    "Authorization": "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20231201/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-date, Signature=signature"
}

# PUT
response = requests.put(url, headers=headers, data=b"Hello, World!")
print(f"PUT Status: {response.status_code}")

# GET
response = requests.get(url, headers=headers)
print(f"GET Status: {response.status_code}")

# DELETE
response = requests.delete(url, headers=headers)
print(f"DELETE Status: {response.status_code}")
```

## Testing

Run the test client to verify the S3 API:

```bash
cd server/tests
python3 s3_test_client.py
```

## Implementation Notes

### Current Limitations

1. **Authentication**: Currently uses hardcoded credentials for testing
2. **Request Modification**: Limited by Actix-Web's HttpRequest immutability
3. **Service Integration**: S3 handlers currently return mock responses
4. **Signature Validation**: Simplified authentication without proper AWS signature calculation

### Future Improvements

1. **Proper AWS Signature Validation**: Implement full AWS4-HMAC-SHA256 signature verification
2. **Credential Management**: Replace hardcoded credentials with proper credential store
3. **Service Integration**: Connect S3 handlers to existing storage services
4. **Request Transformation**: Implement proper request header modification
5. **Error Responses**: Add proper S3-compatible error XML responses

## Architecture

```
S3 Client Request
       ↓
S3 Handlers (s3/handlers.rs)
       ↓
Authentication (s3/auth.rs)
       ↓
Internal Services (service/mod.rs)
       ↓
Storage Layer (storage/mod.rs)
```

## Files

- `s3/mod.rs` - Module exports
- `s3/auth.rs` - Authentication logic
- `s3/handlers.rs` - S3 endpoint handlers
- `s3/middleware.rs` - Request processing middleware
- `tests/s3_integration.rs` - Integration tests
- `tests/s3_test_client.py` - Python test client
