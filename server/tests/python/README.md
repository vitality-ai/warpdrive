# Python S3 Test Clients

Simple test clients for the CIAOS S3-compatible API.

## Files

- `s3_test_client.py` - Basic HTTP client using `requests` library
- `s3_boto_test.py` - AWS SDK client using `boto3` library  
- `requirements.txt` - Python dependencies

## Usage

1. Install dependencies: `pip3 install -r requirements.txt`
2. Start the server: `cargo run` (from server directory)
3. Run tests: `python3 s3_test_client.py` or `python3 s3_boto_test.py`
