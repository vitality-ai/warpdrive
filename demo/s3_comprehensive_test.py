#!/usr/bin/env python3
"""
CIAOS S3 Comprehensive Test
==========================

This script provides a comprehensive test of the S3-compatible API using
pre-generated test files from the test_files/ directory.

Features tested:
- Text files (TXT, JSON)
- Binary files (BIN with various patterns)
- Image files (PNG, JPG, GIF)
- PDF documents
- Large files
- All S3 operations (PUT, GET, DELETE, HEAD, LIST, COPY)

Usage:
    python3 s3_comprehensive_test.py

Requirements:
    pip install boto3 requests
"""

import os
import boto3
import requests
import json
import time
from pathlib import Path

# Configuration
SERVER_URL = "http://localhost:9710"
BUCKET_NAME = "ciaos-test-bucket"
TEST_FILES_DIR = "test_files"
DOWNLOAD_DIR = "downloaded_files"

# S3 Configuration
S3_CONFIG = {
    'endpoint_url': SERVER_URL,
    'aws_access_key_id': 'AKIAIOSFODNN7EXAMPLE',
    'aws_secret_access_key': 'wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY',
    'region_name': 'us-east-1'
}

def create_directories():
    """Create necessary directories"""
    Path(DOWNLOAD_DIR).mkdir(exist_ok=True)
    print(f"✅ Created download directory: {DOWNLOAD_DIR}/")

def get_test_files():
    """Get all test files from test_files directory"""
    test_files = []
    test_files_dir = Path(TEST_FILES_DIR)
    
    if not test_files_dir.exists():
        print(f"❌ Test files directory not found: {TEST_FILES_DIR}")
        return []
    
    for file_path in test_files_dir.iterdir():
        if file_path.is_file():
            test_files.append(str(file_path))
    
    print(f"📁 Found {len(test_files)} test files:")
    for file_path in test_files:
        file_size = Path(file_path).stat().st_size
        print(f"   - {Path(file_path).name} ({file_size:,} bytes)")
    
    return test_files

def test_server_connection():
    """Test if the S3 server is running"""
    try:
        response = requests.get(f"{SERVER_URL}/s3/{BUCKET_NAME}", timeout=5)
        print(f"✅ Server is running at {SERVER_URL}")
        return True
    except requests.exceptions.RequestException as e:
        print(f"❌ Server connection failed: {e}")
        print(f"   Make sure the server is running on {SERVER_URL}")
        return False

def setup_s3_client():
    """Setup S3 client with proper configuration"""
    try:
        s3_client = boto3.client('s3', **S3_CONFIG)
        print("✅ S3 client configured successfully")
        return s3_client
    except Exception as e:
        print(f"❌ Failed to setup S3 client: {e}")
        return None

def upload_files(s3_client, files):
    """Upload files to S3"""
    print(f"\n⬆️  Uploading {len(files)} files to S3...")
    uploaded_files = []
    
    for filepath in files:
        filename = Path(filepath).name
        s3_key = f"test/{filename}"
        
        try:
            print(f"   Uploading: {filename}")
            # Use a small delay to avoid race conditions
            time.sleep(0.1)
            s3_client.upload_file(filepath, BUCKET_NAME, s3_key)
            # Verify upload with a small delay
            time.sleep(0.1)
            s3_client.head_object(Bucket=BUCKET_NAME, Key=s3_key)
            uploaded_files.append(s3_key)
            print(f"   ✅ {filename}")
        except Exception as e:
            print(f"   ❌ {filename}: {e}")
    
    return uploaded_files

def download_files(s3_client, uploaded_files):
    """Download files from S3"""
    print(f"\n⬇️  Downloading {len(uploaded_files)} files from S3...")
    downloaded_files = []
    
    for s3_key in uploaded_files:
        filename = Path(s3_key).name
        download_path = Path(DOWNLOAD_DIR) / filename
        
        try:
            print(f"   Downloading: {filename}")
            # Use a small delay to avoid race conditions
            time.sleep(0.1)
            s3_client.download_file(BUCKET_NAME, s3_key, str(download_path))
            downloaded_files.append(str(download_path))
            print(f"   ✅ {filename}")
        except Exception as e:
            print(f"   ❌ {filename}: {e}")
    
    return downloaded_files

def verify_file_integrity(original_files, downloaded_files):
    """Verify that downloaded files match original files"""
    print(f"\n🔍 Verifying file integrity for {len(original_files)} files...")
    
    all_verified = True
    
    # Create a mapping of original files to downloaded files by name
    original_by_name = {Path(f).name: f for f in original_files}
    downloaded_by_name = {Path(f).name: f for f in downloaded_files}
    
    for original_name, original_path in original_by_name.items():
        original_file = Path(original_path)
        
        if original_name not in downloaded_by_name:
            print(f"❌ Downloaded file not found: {original_name}")
            all_verified = False
            continue
            
        downloaded_file = Path(downloaded_by_name[original_name])
        
        if not downloaded_file.exists():
            print(f"❌ Downloaded file not found: {downloaded_file}")
            all_verified = False
            continue
        
        # Compare file sizes
        original_size = original_file.stat().st_size
        downloaded_size = downloaded_file.stat().st_size
        
        if original_size != downloaded_size:
            print(f"❌ Size mismatch for {original_name}: {original_size} vs {downloaded_size}")
            all_verified = False
            continue
        
        # Compare file contents
        try:
            if original_file.suffix in ['.bin', '.png', '.jpg', '.gif', '.pdf', '.mp4']:
                # Binary file comparison
                original_content = original_file.read_bytes()
                downloaded_content = downloaded_file.read_bytes()
            else:
                # Text file comparison
                original_content = original_file.read_text(encoding='utf-8')
                downloaded_content = downloaded_file.read_text(encoding='utf-8')
            
            if original_content == downloaded_content:
                print(f"✅ {original_name} ({original_size:,} bytes)")
            else:
                print(f"❌ Content mismatch: {original_name}")
                all_verified = False
                
        except Exception as e:
            print(f"❌ Error comparing {original_name}: {e}")
            all_verified = False
    
    return all_verified

def test_s3_operations(s3_client):
    """Test various S3 operations"""
    print(f"\n🧪 Testing S3 operations...")
    
    # Test HEAD operation
    try:
        print("📋 Testing HEAD operation...")
        response = s3_client.head_object(Bucket=BUCKET_NAME, Key="test/sample.txt")
        print(f"✅ HEAD successful: {response['ContentLength']} bytes")
    except Exception as e:
        print(f"❌ HEAD operation failed: {e}")
    
    # Test COPY operation
    try:
        print("📋 Testing COPY operation...")
        source_key = "test/sample.txt"
        dest_key = "test/copied_sample.txt"
        
        copy_source = {'Bucket': BUCKET_NAME, 'Key': source_key}
        s3_client.copy_object(CopySource=copy_source, Bucket=BUCKET_NAME, Key=dest_key)
        
        # Verify copy
        s3_client.head_object(Bucket=BUCKET_NAME, Key=dest_key)
        print(f"✅ COPY successful: {source_key} -> {dest_key}")
        
        # Clean up copied file
        s3_client.delete_object(Bucket=BUCKET_NAME, Key=dest_key)
        print(f"✅ Cleaned up copied file")
        
    except Exception as e:
        print(f"❌ COPY operation failed: {e}")
    
    # Test LIST operation
    try:
        print("📋 Testing LIST operation...")
        response = s3_client.list_objects_v2(Bucket=BUCKET_NAME, Prefix="test/")
        objects = response.get('Contents', [])
        print(f"✅ LIST successful: Found {len(objects)} objects")
        
        for obj in objects[:3]:  # Show first 3 objects
            print(f"   - {obj['Key']} ({obj['Size']:,} bytes)")
        
        if len(objects) > 3:
            print(f"   ... and {len(objects) - 3} more objects")
            
    except Exception as e:
        print(f"❌ LIST operation failed: {e}")

def cleanup_test_files(s3_client, uploaded_files):
    """Clean up test files from S3"""
    print(f"\n🧹 Cleaning up {len(uploaded_files)} test files...")
    
    for s3_key in uploaded_files:
        try:
            s3_client.delete_object(Bucket=BUCKET_NAME, Key=s3_key)
        except Exception as e:
            print(f"❌ Failed to delete {s3_key}: {e}")
    
    print("✅ Cleanup completed")

def main():
    """Main test function"""
    print("🚀 CIAOS S3 Comprehensive Test")
    print("=" * 50)
    
    # Step 1: Create directories
    create_directories()
    
    # Step 2: Test server connection
    if not test_server_connection():
        return
    
    # Step 3: Setup S3 client
    s3_client = setup_s3_client()
    if not s3_client:
        return
    
    # Step 4: Get test files
    test_files = get_test_files()
    if not test_files:
        print("❌ No test files found")
        return
    
    # Step 5: Upload files
    uploaded_files = upload_files(s3_client, test_files)
    
    if not uploaded_files:
        print("❌ No files were uploaded successfully")
        return
    
    # Step 6: Download files
    downloaded_files = download_files(s3_client, uploaded_files)
    
    # Step 7: Verify integrity
    all_verified = verify_file_integrity(test_files, downloaded_files)
    
    # Step 8: Test S3 operations
    test_s3_operations(s3_client)
    
    # Step 9: Cleanup
    cleanup_test_files(s3_client, uploaded_files)
    
    # Final results
    print(f"\n🎉 Comprehensive test completed!")
    print(f"📊 Files tested: {len(test_files)}")
    print(f"✅ Upload successful: {len(uploaded_files)}")
    print(f"✅ Download successful: {len(downloaded_files)}")
    print(f"✅ Integrity verified: {'Yes' if all_verified else 'No'}")
    print(f"📁 Downloaded files available in: {DOWNLOAD_DIR}/")

if __name__ == "__main__":
    main()