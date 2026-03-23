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
    python3 s3_comprehensive_test.py              # upload, test, then delete all objects
    python3 s3_comprehensive_test.py --no-cleanup  # leave objects in bucket (for haystack / Console UI)
    SKIP_CLEANUP=1 python3 s3_comprehensive_test.py  # same as --no-cleanup

Requirements:
    pip install boto3 requests
"""

import os
import sys
import boto3
from botocore.config import Config as BotoConfig
from botocore.exceptions import ClientError
import requests
import json
import time
from pathlib import Path

# Configuration
# Host only (no /s3). Override: WARPDRIVE_ORIGIN=http://other:9710
WARPDRIVE_ORIGIN = os.environ.get("WARPDRIVE_ORIGIN", "http://localhost:9710").rstrip("/")
# boto3 path-style URLs are {endpoint}/{bucket}/{key} → must end with /s3 for Warpdrive routes
S3_ENDPOINT_URL = f"{WARPDRIVE_ORIGIN}/s3"
TEST_FILES_DIR = "test_files"
DOWNLOAD_DIR = "downloaded_files"

# S3 credentials and bucket from demo/test_user_auth.txt (bucket must exist in Vitality Console)
def _load_auth_file():
    auth_path = Path(__file__).resolve().parent / "test_user_auth.txt"
    if not auth_path.exists():
        print("❌ demo/test_user_auth.txt not found.")
        print("   Create an API key in Vitality Console, then create this file with:")
        print("   access_key=<your access key>")
        print("   secret_key=<your secret key>")
        print("   bucket_name=<must exist in Vitality Console for this user, e.g. default>")
        print("   user=<optional, for reference>")
        sys.exit(1)
    creds = {}
    with open(auth_path) as f:
        for line in f:
            line = line.strip()
            if "=" in line and not line.startswith("#"):
                k, v = line.split("=", 1)
                creds[k.strip()] = v.strip()
    access_key = creds.get("access_key", "").strip()
    secret_key = creds.get("secret_key", "").strip()
    if not access_key or not secret_key:
        print("❌ demo/test_user_auth.txt must contain access_key= and secret_key= (from Vitality Console).")
        sys.exit(1)
    bucket_name = creds.get("bucket_name", "").strip() or "default"
    user = creds.get("user", "").strip()
    return access_key, secret_key, bucket_name, user


def _print_401_help():
    print("\n   💡 401 Unauthorized – check:")
    print("      • Vitality Console is running (e.g. http://localhost:8000)")
    print("      • Credentials in demo/test_user_auth.txt match an API key created in Console")
    print("      • WARPDRIVE_SERVICE_SECRET is the same in warpdrive/server/.env and Vitality Console .env")
    print()


def _print_bucket_check_help():
    print("\n   💡 Wrong/missing bucket is NOT rejected? Check:")
    print("      • Rebuild & restart Vitality Console **backend** so POST /api/auth/s3-credentials")
    print("        accepts JSON field `bucket` (older images ignored it — random buckets looked “allowed”).")
    print("      • docker compose: docker compose build console-backend --no-cache && docker compose up -d")
    print("      • This script uses path-style S3 URLs (see BotoConfig addressing_style).")
    print()


_ACCESS_KEY, _SECRET_KEY, BUCKET_NAME, _USER = _load_auth_file()
# Path-style: PUT http://host:9710/s3/{bucket}/{key} — required for Warpdrive routes.
# Without this, some boto3/botocore versions use virtual-hosted style and the bucket
# may not appear in /s3/{bucket}/..., so Console never receives bucket for verification.
S3_CONFIG = {
    "endpoint_url": S3_ENDPOINT_URL,
    "aws_access_key_id": _ACCESS_KEY,
    "aws_secret_access_key": _SECRET_KEY,
    "region_name": "us-east-1",
    "config": BotoConfig(
        signature_version="s3v4",
        s3={"addressing_style": "path"},
    ),
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
    """Test if Warpdrive is reachable (GET /s3 may return 401 without auth — still means server is up)."""
    try:
        r = requests.get(f"{WARPDRIVE_ORIGIN}/s3", timeout=5)
        print(f"✅ Warpdrive reachable at {WARPDRIVE_ORIGIN} (GET /s3 → HTTP {r.status_code})")
        return True
    except requests.exceptions.RequestException as e:
        print(f"❌ Server connection failed: {e}")
        print(f"   Make sure Warpdrive is running on {WARPDRIVE_ORIGIN}")
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
    _401_help_shown = [False]  # use list so inner function can set it

    for filepath in files:
        filename = Path(filepath).name
        s3_key = f"test/{filename}"

        try:
            print(f"   Uploading: {filename}")
            time.sleep(0.1)
            s3_client.upload_file(filepath, BUCKET_NAME, s3_key)
            time.sleep(0.1)
            s3_client.head_object(Bucket=BUCKET_NAME, Key=s3_key)
            uploaded_files.append(s3_key)
            print(f"   ✅ {filename}")
        except ClientError as e:
            print(f"   ❌ {filename}: {e}")
            err = e.response.get("Error", {}) if e.response else {}
            code = err.get("Code", "")
            if code in ("403", "AccessDenied") or "403" in str(e):
                _print_bucket_check_help()
            if not _401_help_shown[0] and ("401" in str(e) or code == "401"):
                _print_401_help()
                _401_help_shown[0] = True
        except Exception as e:
            print(f"   ❌ {filename}: {e}")
            if not _401_help_shown[0] and ("401" in str(e) or "Unauthorized" in str(e)):
                _print_401_help()
                _401_help_shown[0] = True

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
        #s3_client.delete_object(Bucket=BUCKET_NAME, Key=dest_key)
        #print(f"✅ Cleaned up copied file")
        
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
        if "401" in str(e) or "Unauthorized" in str(e):
            _print_401_help()

def cleanup_test_files(s3_client, uploaded_files, skip=False):
    """Clean up test files from S3. If skip=True, leaves objects in place (so they appear in haystack / Console UI)."""
    if skip:
        print(f"\n⏭️  Skipping cleanup (--no-cleanup): {len(uploaded_files)} objects left in bucket for inspection.")
        return
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
    print(f"   Credentials: test_user_auth.txt (access_key={_ACCESS_KEY[:8]}...)")
    print(f"   Bucket: {BUCKET_NAME}" + (f" (user={_USER})" if _USER else ""))
    print(f"   S3 endpoint: {S3_ENDPOINT_URL}")
    print()
    print("ℹ️  Bucket must exist in Vitality Console")
    print("   Warpdrive asks Console to verify this bucket for your API key (except ListBuckets).")
    print("   Create the bucket in the Console UI (or use `default`) or S3 calls will fail with 403.")
    print()

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
    
    # Step 9: Cleanup (unless --no-cleanup or SKIP_CLEANUP=1)
    skip_cleanup = "--no-cleanup" in sys.argv or os.environ.get("SKIP_CLEANUP", "").strip().lower() in ("1", "true", "yes")
    #cleanup_test_files(s3_client, uploaded_files, skip=skip_cleanup)
    
    # Final results
    print(f"\n🎉 Comprehensive test completed!")
    print(f"📊 Files tested: {len(test_files)}")
    print(f"✅ Upload successful: {len(uploaded_files)}")
    print(f"✅ Download successful: {len(downloaded_files)}")
    print(f"✅ Integrity verified: {'Yes' if all_verified else 'No'}")
    print(f"📁 Downloaded files available in: {DOWNLOAD_DIR}/")

if __name__ == "__main__":
    main()