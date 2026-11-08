# Python S3 Test Clients

Simple test clients for the CIAOS S3-compatible API.

## Files

- `s3_test_client.py` - Basic HTTP client using `requests` library
- `s3_boto_test.py` - AWS SDK client using `boto3` library  
- `post_object_test.py` - POST Object (RFC 2.14, HTML form upload) test client; single-node only, uses `requests` directly since the signed-policy-document auth scheme isn't something boto3 has a client method for
- `sts_oidc_test.py` - STS-compatible `AssumeRoleWithWebIdentity` multi-tenant test client; creates its own test realm/client/mapper/users against a running Keycloak (idempotent -- safe to re-run), mints real OIDC tokens for several distinct identities (each with their own per-user bucket claim via a real Keycloak protocol mapper, not a hardcoded value), and exercises WarpDrive's `/sts` endpoint with an unmodified boto3 STS client -- including a live cross-tenant `CopyObject` scope-bypass check. Runs in CI as `sts-oidc-tests` in `.github/workflows/testcase-workflow.yml`.
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

## STS/OIDC federation

`sts_oidc_test.py` needs a running Keycloak (`KEYCLOAK_URL`, default
`http://localhost:8080`) and a WarpDrive server started with
`WARPDRIVE_OIDC_ISSUER`/`WARPDRIVE_OIDC_AUDIENCE`/`WARPDRIVE_OIDC_JWKS_URL`
pointed at the realm it creates (`warpdrive` by default), and
`WARPDRIVE_OIDC_BUCKETS_CLAIM=buckets` so each test user's own per-user
claim (not just the server-wide default) actually drives their session's
scope -- this is what makes the multi-tenant isolation checks meaningful.
See [docs/user_guide.md](../../../docs/user_guide.md#federated-auth-via-your-own-oidc-provider-sts).
Locally:

```
docker run -d --name keycloak -p 8080:8080 \
  -e KEYCLOAK_ADMIN=admin -e KEYCLOAK_ADMIN_PASSWORD=admin \
  quay.io/keycloak/keycloak:latest start-dev

WARPDRIVE_OIDC_ISSUER="http://localhost:8080/realms/warpdrive" \
WARPDRIVE_OIDC_AUDIENCE="warpdrive-client" \
WARPDRIVE_OIDC_JWKS_URL="http://localhost:8080/realms/warpdrive/protocol/openid-connect/certs" \
WARPDRIVE_OIDC_BUCKETS_CLAIM="buckets" \
WARPDRIVE_OIDC_DEFAULT_BUCKETS="sts-tenant-default" \
  cargo run --release

python3 sts_oidc_test.py
```
