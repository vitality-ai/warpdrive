#!/usr/bin/env python3
"""
S3-compatible API test client for CIAOS
This script demonstrates how to use the S3-compatible endpoints
"""

import requests
import json
import sys

# Server configuration
SERVER_URL = "http://localhost:9710"
S3_BASE_URL = f"{SERVER_URL}/s3"

# S3 Authentication credentials (hardcoded for testing)
ACCESS_KEY = "AKIAIOSFODNN7EXAMPLE"
SECRET_KEY = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"

# Test bucket and key
TEST_BUCKET = "test-bucket"
TEST_KEY = "test-object"

def create_auth_header():
    """Create a simple AWS4 authorization header for testing"""
    # This is a simplified version - in production, you'd need proper AWS signature calculation
    return f"AWS4-HMAC-SHA256 Credential={ACCESS_KEY}/20231201/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-date, Signature=signature"

def test_s3_put():
    """Test S3 PUT object"""
    print("Testing S3 PUT object...")
    
    url = f"{S3_BASE_URL}/{TEST_BUCKET}/{TEST_KEY}"
    headers = {
        "Authorization": create_auth_header(),
        "Content-Type": "application/octet-stream"
    }
    data = b"Hello, S3 World! This is a test object."
    
    try:
        response = requests.put(url, headers=headers, data=data)
        print(f"PUT Response Status: {response.status_code}")
        print(f"PUT Response Body: {response.text}")
        return response.status_code == 200
    except requests.exceptions.ConnectionError:
        print("Error: Could not connect to server. Make sure the server is running on port 9710")
        return False
    except Exception as e:
        print(f"Error: {e}")
        return False

def test_s3_get():
    """Test S3 GET object"""
    print("\nTesting S3 GET object...")
    
    url = f"{S3_BASE_URL}/{TEST_BUCKET}/{TEST_KEY}"
    headers = {
        "Authorization": create_auth_header()
    }
    
    try:
        response = requests.get(url, headers=headers)
        print(f"GET Response Status: {response.status_code}")
        print(f"GET Response Body: {response.text}")
        return response.status_code == 200
    except requests.exceptions.ConnectionError:
        print("Error: Could not connect to server. Make sure the server is running on port 9710")
        return False
    except Exception as e:
        print(f"Error: {e}")
        return False

def test_s3_delete():
    """Test S3 DELETE object"""
    print("\nTesting S3 DELETE object...")
    
    url = f"{S3_BASE_URL}/{TEST_BUCKET}/{TEST_KEY}"
    headers = {
        "Authorization": create_auth_header()
    }
    
    try:
        response = requests.delete(url, headers=headers)
        print(f"DELETE Response Status: {response.status_code}")
        print(f"DELETE Response Body: {response.text}")
        return response.status_code == 200
    except requests.exceptions.ConnectionError:
        print("Error: Could not connect to server. Make sure the server is running on port 9710")
        return False
    except Exception as e:
        print(f"Error: {e}")
        return False

def test_s3_head():
    """Test S3 HEAD object"""
    print("\nTesting S3 HEAD object...")
    
    url = f"{S3_BASE_URL}/{TEST_BUCKET}/{TEST_KEY}"
    headers = {
        "Authorization": create_auth_header()
    }
    
    try:
        response = requests.head(url, headers=headers)
        print(f"HEAD Response Status: {response.status_code}")
        print(f"HEAD Response Headers: {dict(response.headers)}")
        return response.status_code == 200
    except requests.exceptions.ConnectionError:
        print("Error: Could not connect to server. Make sure the server is running on port 9710")
        return False
    except Exception as e:
        print(f"Error: {e}")
        return False

def test_s3_list():
    """Test S3 List objects"""
    print("\nTesting S3 List objects...")
    
    url = f"{S3_BASE_URL}/{TEST_BUCKET}"
    headers = {
        "Authorization": create_auth_header()
    }
    params = {"list-type": "2"}
    
    try:
        response = requests.get(url, headers=headers, params=params)
        print(f"LIST Response Status: {response.status_code}")
        print(f"LIST Response Body: {response.text}")
        return response.status_code == 200
    except requests.exceptions.ConnectionError:
        print("Error: Could not connect to server. Make sure the server is running on port 9710")
        return False
    except Exception as e:
        print(f"Error: {e}")
        return False

def test_authentication_failure():
    """Test authentication with invalid credentials"""
    print("\nTesting authentication failure...")
    
    url = f"{S3_BASE_URL}/{TEST_BUCKET}/{TEST_KEY}"
    headers = {
        "Authorization": "AWS4-HMAC-SHA256 Credential=INVALID_KEY/20231201/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-date, Signature=signature"
    }
    
    try:
        response = requests.get(url, headers=headers)
        print(f"Auth Failure Response Status: {response.status_code}")
        print(f"Auth Failure Response Body: {response.text}")
        return response.status_code == 401
    except requests.exceptions.ConnectionError:
        print("Error: Could not connect to server. Make sure the server is running on port 9710")
        return False
    except Exception as e:
        print(f"Error: {e}")
        return False

def main():
    """Run all S3 API tests"""
    print("=== CIAOS S3-Compatible API Test Client ===")
    print(f"Server URL: {SERVER_URL}")
    print(f"S3 Base URL: {S3_BASE_URL}")
    print(f"Test Bucket: {TEST_BUCKET}")
    print(f"Test Key: {TEST_KEY}")
    print("=" * 50)
    
    tests = [
        ("S3 PUT Object", test_s3_put),
        ("S3 GET Object", test_s3_get),
        ("S3 DELETE Object", test_s3_delete),
        ("S3 HEAD Object", test_s3_head),
        ("S3 List Objects", test_s3_list),
        ("Authentication Failure", test_authentication_failure),
    ]
    
    results = []
    for test_name, test_func in tests:
        print(f"\n--- {test_name} ---")
        try:
            result = test_func()
            results.append((test_name, result))
            print(f"Result: {'PASS' if result else 'FAIL'}")
        except Exception as e:
            print(f"Test failed with exception: {e}")
            results.append((test_name, False))
    
    print("\n" + "=" * 50)
    print("TEST SUMMARY:")
    print("=" * 50)
    
    passed = 0
    total = len(results)
    
    for test_name, result in results:
        status = "PASS" if result else "FAIL"
        print(f"{test_name}: {status}")
        if result:
            passed += 1
    
    print(f"\nTotal: {passed}/{total} tests passed")
    
    if passed == total:
        print("🎉 All tests passed!")
        return 0
    else:
        print("❌ Some tests failed!")
        return 1

if __name__ == "__main__":
    sys.exit(main())
