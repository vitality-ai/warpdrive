# Python S3 Test Clients

Simple test clients for the CIAOS S3-compatible API.

## Files

- `s3_test_client.py` - Basic HTTP client using `requests` library
- `s3_boto_test.py` - AWS SDK client using `boto3` library  
- `cluster_checksum_test.py` - Checksum-by-default + read-side corruption detection against the distributed/cluster API (plain and FAC/packed paths)
- `requirements.txt` - Python dependencies

## Usage

1. Install dependencies: `pip3 install -r requirements.txt`
2. Start the server: `cargo run` (from server directory)
3. Run tests: `python3 s3_test_client.py` or `python3 s3_boto_test.py`

`cluster_checksum_test.py` targets the cluster API instead of the single-node
one, so it needs a local multi-node cluster rather than a single `cargo run`:

```
(cd ../.. && cargo build --release --bin warp_drive)
../../../scripts/launch_local_cluster.sh 5 9710 /tmp/wd-local-cluster
python3 cluster_checksum_test.py
```
