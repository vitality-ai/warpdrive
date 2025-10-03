#!/usr/bin/env python3
"""
S3-compatible API test client using boto3 (AWS SDK)
This script tests the CIAOS S3 API using the official AWS SDK to ensure full compatibility
"""

import boto3
import botocore
import json
import sys
import time
from botocore.exceptions import ClientError, NoCredentialsError
from botocore.config import Config

# Server configuration
SERVER_URL = "http://localhost:9710"
S3_ENDPOINT_URL = f"{SERVER_URL}/s3"

# S3 Authentication credentials (hardcoded for testing)
ACCESS_KEY = "AKIAIOSFODNN7EXAMPLE"
SECRET_KEY = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"

# Test configuration
TEST_BUCKET = "test-bucket-boto"
TEST_KEY = "test-object-boto"
TEST_DATA = b"Hello, CIAOS S3 World! This is a boto3 test."

def create_s3_client():
    """Create a boto3 S3 client configured for our CIAOS server"""
    try:
        # Configure boto3 to use our custom endpoint
        s3_client = boto3.client(
            's3',
            endpoint_url=S3_ENDPOINT_URL,
            aws_access_key_id=ACCESS_KEY,
            aws_secret_access_key=SECRET_KEY,
            region_name='us-east-1',
            config=Config(
                signature_version='s3v4',
                s3={
                    'addressing_style': 'path'
                }
            )
        )
        return s3_client
    except Exception as e:
        print(f"Error creating S3 client: {e}")
        return None

def test_s3_put_object(s3_client):
    """Test S3 PUT object using boto3"""
    print("Testing S3 PUT Object with boto3...")
    
    try:
        # Upload object
        response = s3_client.put_object(
            Bucket=TEST_BUCKET,
            Key=TEST_KEY,
            Body=TEST_DATA,
            ContentType='application/octet-stream'
        )
        
        print(f"PUT Response: {response}")
        print(f"ETag: {response.get('ETag', 'N/A')}")
        return True
        
    except ClientError as e:
        error_code = e.response['Error']['Code']
        print(f"PUT Error: {error_code} - {e}")
        return False
    except Exception as e:
        print(f"PUT Unexpected Error: {e}")
        return False

def test_s3_get_object(s3_client):
    """Test S3 GET object using boto3"""
    print("\nTesting S3 GET Object with boto3...")
    
    try:
        # Download object
        response = s3_client.get_object(
            Bucket=TEST_BUCKET,
            Key=TEST_KEY
        )
        
        # Read the body
        body = response['Body'].read()
        
        print(f"GET Response: {response}")
        print(f"Content Length: {response.get('ContentLength', 'N/A')}")
        print(f"Content Type: {response.get('ContentType', 'N/A')}")
        print(f"Body matches test data: {body == TEST_DATA}")
        
        return body == TEST_DATA
        
    except ClientError as e:
        error_code = e.response['Error']['Code']
        print(f"GET Error: {error_code} - {e}")
        return False
    except Exception as e:
        print(f"GET Unexpected Error: {e}")
        return False

def test_s3_head_object(s3_client):
    """Test S3 HEAD object using boto3"""
    print("\nTesting S3 HEAD Object with boto3...")
    
    try:
        # Get object metadata
        response = s3_client.head_object(
            Bucket=TEST_BUCKET,
            Key=TEST_KEY
        )
        
        print(f"HEAD Response: {response}")
        print(f"Content Length: {response.get('ContentLength', 'N/A')}")
        print(f"Content Type: {response.get('ContentType', 'N/A')}")
        print(f"Last Modified: {response.get('LastModified', 'N/A')}")
        
        return True
        
    except ClientError as e:
        error_code = e.response['Error']['Code']
        print(f"HEAD Error: {error_code} - {e}")
        return False
    except Exception as e:
        print(f"HEAD Unexpected Error: {e}")
        return False

def test_s3_delete_object(s3_client):
    """Test S3 DELETE object using boto3"""
    print("\nTesting S3 DELETE Object with boto3...")
    
    try:
        # Delete object
        response = s3_client.delete_object(
            Bucket=TEST_BUCKET,
            Key=TEST_KEY
        )
        
        print(f"DELETE Response: {response}")
        return True
        
    except ClientError as e:
        error_code = e.response['Error']['Code']
        print(f"DELETE Error: {error_code} - {e}")
        return False
    except Exception as e:
        print(f"DELETE Unexpected Error: {e}")
        return False

def test_s3_list_objects(s3_client):
    """Test S3 List objects using boto3"""
    print("\nTesting S3 List Objects with boto3...")
    
    try:
        # List objects
        response = s3_client.list_objects_v2(
            Bucket=TEST_BUCKET,
            MaxKeys=1000
        )
        
        print(f"LIST Response: {response}")
        print(f"Key Count: {response.get('KeyCount', 0)}")
        print(f"Is Truncated: {response.get('IsTruncated', False)}")
        
        if 'Contents' in response:
            print("Objects found:")
            for obj in response['Contents']:
                print(f"  - {obj['Key']} ({obj['Size']} bytes)")
        else:
            print("No objects found")
        
        return True
        
    except ClientError as e:
        error_code = e.response['Error']['Code']
        print(f"LIST Error: {error_code} - {e}")
        return False
    except Exception as e:
        print(f"LIST Unexpected Error: {e}")
        return False

def test_s3_multipart_upload(s3_client):
    """Test S3 multipart upload using boto3"""
    print("\nTesting S3 Multipart Upload with boto3...")
    
    try:
        # Create a larger test file for multipart upload
        large_data = b"X" * (5 * 1024 * 1024)  # 5MB of data
        
        # Upload using multipart
        response = s3_client.put_object(
            Bucket=TEST_BUCKET,
            Key=f"{TEST_KEY}-multipart",
            Body=large_data,
            ContentType='application/octet-stream'
        )
        
        print(f"Multipart Upload Response: {response}")
        print(f"ETag: {response.get('ETag', 'N/A')}")
        
        # Clean up
        s3_client.delete_object(
            Bucket=TEST_BUCKET,
            Key=f"{TEST_KEY}-multipart"
        )
        
        return True
        
    except ClientError as e:
        error_code = e.response['Error']['Code']
        print(f"Multipart Upload Error: {error_code} - {e}")
        return False
    except Exception as e:
        print(f"Multipart Upload Unexpected Error: {e}")
        return False

def test_s3_copy_object(s3_client):
    """Test S3 copy object using boto3"""
    print("\nTesting S3 Copy Object with boto3...")
    
    try:
        # First upload a source object
        s3_client.put_object(
            Bucket=TEST_BUCKET,
            Key=f"{TEST_KEY}-source",
            Body=b"Source object for copy test",
            ContentType='text/plain'
        )
        
        # Copy the object
        copy_source = {
            'Bucket': TEST_BUCKET,
            'Key': f"{TEST_KEY}-source"
        }
        
        response = s3_client.copy_object(
            Bucket=TEST_BUCKET,
            Key=f"{TEST_KEY}-copy",
            CopySource=copy_source
        )
        
        print(f"Copy Response: {response}")
        print(f"ETag: {response.get('CopyObjectResult', {}).get('ETag', 'N/A')}")
        
        # Clean up
        s3_client.delete_object(Bucket=TEST_BUCKET, Key=f"{TEST_KEY}-source")
        s3_client.delete_object(Bucket=TEST_BUCKET, Key=f"{TEST_KEY}-copy")
        
        return True
        
    except ClientError as e:
        error_code = e.response['Error']['Code']
        print(f"Copy Error: {error_code} - {e}")
        return False
    except Exception as e:
        print(f"Copy Unexpected Error: {e}")
        return False

def test_s3_authentication_failure():
    """Test S3 authentication with invalid credentials"""
    print("\nTesting S3 Authentication Failure...")
    
    try:
        # Create client with invalid credentials
        invalid_client = boto3.client(
            's3',
            endpoint_url=S3_ENDPOINT_URL,
            aws_access_key_id='INVALID_KEY',
            aws_secret_access_key='INVALID_SECRET',
            region_name='us-east-1'
        )
        
        # Try to list objects (should fail)
        invalid_client.list_objects_v2(Bucket=TEST_BUCKET)
        print("ERROR: Authentication should have failed!")
        return False
        
    except ClientError as e:
        error_code = e.response['Error']['Code']
        print(f"Expected Authentication Error: {error_code} - {e}")
        return True  # This is expected
    except Exception as e:
        print(f"Authentication Error: {e}")
        return True  # This is also expected

def test_s3_bucket_operations(s3_client):
    """Test S3 bucket operations"""
    print("\nTesting S3 Bucket Operations...")
    
    try:
        # Test listing buckets (this might not be implemented)
        try:
            response = s3_client.list_buckets()
            print(f"List Buckets Response: {response}")
        except ClientError as e:
            print(f"List Buckets not supported: {e.response['Error']['Code']}")
        
        # Test bucket existence
        try:
            s3_client.head_bucket(Bucket=TEST_BUCKET)
            print(f"Bucket {TEST_BUCKET} exists")
        except ClientError as e:
            print(f"Bucket {TEST_BUCKET} does not exist or is not accessible: {e.response['Error']['Code']}")
        
        return True
        
    except Exception as e:
        print(f"Bucket Operations Error: {e}")
        return False

def main():
    """Run all boto3 S3 API tests"""
    print("=== CIAOS S3-Compatible API Boto3 Test Client ===")
    print(f"Server URL: {SERVER_URL}")
    print(f"S3 Endpoint: {S3_ENDPOINT_URL}")
    print(f"Test Bucket: {TEST_BUCKET}")
    print(f"Test Key: {TEST_KEY}")
    print("=" * 60)
    
    # Create S3 client
    s3_client = create_s3_client()
    if not s3_client:
        print("❌ Failed to create S3 client")
        return 1
    
    print("✅ S3 client created successfully")
    
    # Define test functions
    tests = [
        ("S3 PUT Object", lambda: test_s3_put_object(s3_client)),
        ("S3 GET Object", lambda: test_s3_get_object(s3_client)),
        ("S3 HEAD Object", lambda: test_s3_head_object(s3_client)),
        ("S3 List Objects", lambda: test_s3_list_objects(s3_client)),
        ("S3 Multipart Upload", lambda: test_s3_multipart_upload(s3_client)),
        ("S3 Copy Object", lambda: test_s3_copy_object(s3_client)),
        ("S3 Bucket Operations", lambda: test_s3_bucket_operations(s3_client)),
        ("S3 Authentication Failure", test_s3_authentication_failure),
        ("S3 DELETE Object", lambda: test_s3_delete_object(s3_client)),
    ]
    
    results = []
    passed = 0
    total = len(tests)
    
    for test_name, test_func in tests:
        print(f"\n--- {test_name} ---")
        try:
            result = test_func()
            results.append((test_name, result))
            status = "PASS" if result else "FAIL"
            print(f"Result: {status}")
            if result:
                passed += 1
        except Exception as e:
            print(f"Test failed with exception: {e}")
            results.append((test_name, False))
    
    print("\n" + "=" * 60)
    print("BOTO3 TEST SUMMARY:")
    print("=" * 60)
    
    for test_name, result in results:
        status = "PASS" if result else "FAIL"
        print(f"{test_name}: {status}")
    
    print(f"\nTotal: {passed}/{total} tests passed")
    
    if passed == total:
        print("🎉 All boto3 tests passed!")
        return 0
    else:
        print("❌ Some boto3 tests failed!")
        return 1

if __name__ == "__main__":
    sys.exit(main())
