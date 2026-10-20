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

The same `s3_boto_test.py` suite also runs against the distributed/cluster
engine, unchanged, just pointed at the cluster's S3 surface instead of the
single-node one. Start a cluster the normal way (see
[docs/user_guide.md](../../../docs/user_guide.md#starting-a-cluster) -- one
`cargo run` per node with `WARPDRIVE_PEERS` set), then:

```
WARPDRIVE_S3_PATH=/cluster/s3 python3 s3_boto_test.py
```

`WARPDRIVE_SERVER_URL` (default `http://localhost:9710`) points at any node
in the cluster. No separate test file or setup script for this -- it's the
same assertions against the same base URL scheme, just a different path
prefix.
