#!/usr/bin/env python3
"""
STS/OIDC federation test client -- exercises WarpDrive's POST /sts
(AssumeRoleWithWebIdentity) against a real Keycloak instance, using an
unmodified boto3 STS client exactly as a real integration would.

Requires a running Keycloak (KEYCLOAK_URL, default http://localhost:8080)
and a running WarpDrive server configured to trust it (WARPDRIVE_OIDC_ISSUER/
AUDIENCE/JWKS_URL pointed at the same realm this script creates -- see
docs/user_guide.md#federated-auth-via-your-own-oidc-provider-sts).

This script owns realm/client/user setup itself (idempotent -- a 409 from
an already-existing realm/client/user is treated as success, not a
failure), so the same script works for a fresh CI Keycloak container and
for a developer re-running it locally without tearing anything down first.
"""

import os
import sys
import boto3
import botocore
import requests

KEYCLOAK_URL = os.environ.get("KEYCLOAK_URL", "http://localhost:8080")
KEYCLOAK_ADMIN_USER = os.environ.get("KEYCLOAK_ADMIN_USER", "admin")
KEYCLOAK_ADMIN_PASSWORD = os.environ.get("KEYCLOAK_ADMIN_PASSWORD", "admin")
REALM = os.environ.get("WARPDRIVE_STS_TEST_REALM", "warpdrive")
CLIENT_ID = os.environ.get("WARPDRIVE_STS_TEST_CLIENT_ID", "warpdrive-client")
TEST_USERNAME = "alice"
TEST_PASSWORD = "alicepass"

SERVER_URL = os.environ.get("WARPDRIVE_SERVER_URL", "http://localhost:9710")
TEST_BUCKET = os.environ.get("WARPDRIVE_STS_TEST_BUCKET", "sts-test-bucket")


def get_admin_token():
    resp = requests.post(
        f"{KEYCLOAK_URL}/realms/master/protocol/openid-connect/token",
        data={
            "client_id": "admin-cli",
            "username": KEYCLOAK_ADMIN_USER,
            "password": KEYCLOAK_ADMIN_PASSWORD,
            "grant_type": "password",
        },
        timeout=10,
    )
    resp.raise_for_status()
    return resp.json()["access_token"]


def setup_keycloak(admin_token):
    """Idempotent: creates the test realm/client/user if they don't already exist."""
    headers = {"Authorization": f"Bearer {admin_token}", "Content-Type": "application/json"}

    r = requests.post(f"{KEYCLOAK_URL}/admin/realms", headers=headers, json={"realm": REALM, "enabled": True}, timeout=10)
    if r.status_code not in (201, 409):
        r.raise_for_status()

    r = requests.post(
        f"{KEYCLOAK_URL}/admin/realms/{REALM}/clients",
        headers=headers,
        json={
            "clientId": CLIENT_ID,
            "enabled": True,
            "publicClient": True,
            "directAccessGrantsEnabled": True,
            "standardFlowEnabled": False,
        },
        timeout=10,
    )
    if r.status_code not in (201, 409):
        r.raise_for_status()

    # email/firstName/lastName are required here -- Keycloak's default User
    # Profile config implicitly triggers a VERIFY_PROFILE required action
    # (surfaced as the cryptic "Account is not fully set up" / invalid_grant
    # on the password grant) if they're missing, even with requiredActions
    # explicitly set to [].
    r = requests.post(
        f"{KEYCLOAK_URL}/admin/realms/{REALM}/users",
        headers=headers,
        json={
            "username": TEST_USERNAME,
            "enabled": True,
            "emailVerified": True,
            "email": "alice@example.com",
            "firstName": "Alice",
            "lastName": "Test",
            "requiredActions": [],
            "credentials": [{"type": "password", "value": TEST_PASSWORD, "temporary": False}],
        },
        timeout=10,
    )
    if r.status_code not in (201, 409):
        r.raise_for_status()


def get_id_token():
    resp = requests.post(
        f"{KEYCLOAK_URL}/realms/{REALM}/protocol/openid-connect/token",
        data={
            "client_id": CLIENT_ID,
            "username": TEST_USERNAME,
            "password": TEST_PASSWORD,
            "grant_type": "password",
            "scope": "openid",
        },
        timeout=10,
    )
    resp.raise_for_status()
    return resp.json()["id_token"]


def test_assume_role_and_s3_round_trip(id_token):
    sts = boto3.client(
        "sts",
        endpoint_url=f"{SERVER_URL}/sts",
        aws_access_key_id="unused",
        aws_secret_access_key="unused",
        region_name="us-east-1",
    )
    resp = sts.assume_role_with_web_identity(
        RoleArn="arn:aws:iam::000000000000:role/warpdrive-sts-test",
        RoleSessionName="ci-sts-oidc-test",
        WebIdentityToken=id_token,
        DurationSeconds=1800,
    )
    creds = resp["Credentials"]
    if not resp.get("SubjectFromWebIdentityToken"):
        print("  missing SubjectFromWebIdentityToken in response")
        return False

    s3 = boto3.client(
        "s3",
        endpoint_url=f"{SERVER_URL}/s3",
        aws_access_key_id=creds["AccessKeyId"],
        aws_secret_access_key=creds["SecretAccessKey"],
        aws_session_token=creds["SessionToken"],
        region_name="us-east-1",
        config=botocore.config.Config(s3={"addressing_style": "path"}),
    )

    s3.create_bucket(Bucket=TEST_BUCKET)
    s3.put_object(Bucket=TEST_BUCKET, Key="hello.txt", Body=b"hello from an OIDC-federated session")
    body = s3.get_object(Bucket=TEST_BUCKET, Key="hello.txt")["Body"].read()
    return body == b"hello from an OIDC-federated session"


def test_bucket_scope_enforced(id_token):
    sts = boto3.client(
        "sts", endpoint_url=f"{SERVER_URL}/sts",
        aws_access_key_id="unused", aws_secret_access_key="unused", region_name="us-east-1",
    )
    creds = sts.assume_role_with_web_identity(
        RoleArn="arn:aws:iam::000000000000:role/warpdrive-sts-test",
        RoleSessionName="ci-sts-oidc-test-scope",
        WebIdentityToken=id_token,
    )["Credentials"]
    s3 = boto3.client(
        "s3", endpoint_url=f"{SERVER_URL}/s3",
        aws_access_key_id=creds["AccessKeyId"], aws_secret_access_key=creds["SecretAccessKey"],
        aws_session_token=creds["SessionToken"], region_name="us-east-1",
        config=botocore.config.Config(s3={"addressing_style": "path"}),
    )
    try:
        s3.create_bucket(Bucket="not-in-scope-bucket")
        return False  # should have been rejected
    except botocore.exceptions.ClientError as e:
        return e.response["Error"]["Code"] in ("AccessDenied", "403", "Forbidden")


def test_tampered_token_rejected(id_token):
    sts = boto3.client(
        "sts", endpoint_url=f"{SERVER_URL}/sts",
        aws_access_key_id="unused", aws_secret_access_key="unused", region_name="us-east-1",
    )
    tampered = id_token[:-5] + "AAAAA"
    try:
        sts.assume_role_with_web_identity(
            RoleArn="arn:aws:iam::000000000000:role/warpdrive-sts-test",
            RoleSessionName="ci-sts-oidc-test-tampered",
            WebIdentityToken=tampered,
        )
        return False  # should have been rejected
    except botocore.exceptions.ClientError as e:
        return e.response["Error"]["Code"] == "InvalidIdentityToken"


def test_unknown_access_key_rejected():
    s3 = boto3.client(
        "s3", endpoint_url=f"{SERVER_URL}/s3",
        aws_access_key_id="ASIA_NOT_A_REAL_SESSION", aws_secret_access_key="whatever",
        aws_session_token="whatever", region_name="us-east-1",
        config=botocore.config.Config(s3={"addressing_style": "path"}),
    )
    try:
        s3.list_objects_v2(Bucket=TEST_BUCKET)
        return False  # should have been rejected
    except botocore.exceptions.ClientError:
        return True


def main():
    print(f"Keycloak: {KEYCLOAK_URL}  WarpDrive: {SERVER_URL}")

    admin_token = get_admin_token()
    setup_keycloak(admin_token)
    id_token = get_id_token()
    print("Minted a real Keycloak id_token for test user 'alice'")

    tests = [
        ("AssumeRoleWithWebIdentity + S3 round trip", lambda: test_assume_role_and_s3_round_trip(id_token)),
        ("Bucket-scope enforcement (403 outside scope)", lambda: test_bucket_scope_enforced(id_token)),
        ("Tampered WebIdentityToken rejected", lambda: test_tampered_token_rejected(id_token)),
        ("Unknown access key rejected", test_unknown_access_key_rejected),
    ]

    passed = 0
    for name, fn in tests:
        print(f"\n--- {name} ---")
        try:
            ok = fn()
        except Exception as e:
            print(f"  exception: {e}")
            ok = False
        print("PASS" if ok else "FAIL")
        passed += ok

    print(f"\n{passed}/{len(tests)} STS/OIDC tests passed")
    return 0 if passed == len(tests) else 1


if __name__ == "__main__":
    sys.exit(main())
