#!/usr/bin/env python3
"""
STS/OIDC federation test client -- exercises WarpDrive's POST /sts
(AssumeRoleWithWebIdentity) against a real Keycloak instance, using an
unmodified boto3 STS client exactly as a real integration would.

Requires a running Keycloak (KEYCLOAK_URL, default http://localhost:8080)
and a running WarpDrive server configured to trust it:
WARPDRIVE_OIDC_ISSUER/AUDIENCE/JWKS_URL pointed at the realm this script
creates, WARPDRIVE_OIDC_BUCKETS_CLAIM=buckets (so each test user's own
per-user claim actually drives their session's scope -- see
docs/user_guide.md#federated-auth-via-your-own-oidc-provider-sts), and
WARPDRIVE_OIDC_DEFAULT_BUCKETS set to something other than any test user's
own bucket (used by the claim-absent fallback scenario below).

This is multi-tenant correctness testing, not single-user smoke testing:
three distinct Keycloak identities, each covering a different branch of
resolve_allowed_buckets() (server/src/s3/sts.rs) and a different way two
tenants sharing one WarpDrive server could leak into each other:

- alice / bob: two ordinary tenants, each with their own "buckets" claim
  (via a real Keycloak user-attribute protocol mapper, not a hardcoded
  claim) pointing at their own, different bucket. Used to prove: each can
  operate in their own scope, neither can touch the other's bucket
  (including via CopyObject's source-bucket path specifically), their
  sessions get genuinely distinct identities, and a tampered token or an
  unknown access key is still rejected.
- carol: no "buckets" attribute at all -- exercises the claim-absent
  fallback to WARPDRIVE_OIDC_DEFAULT_BUCKETS, and confirms that fallback
  scope still can't reach alice's or bob's bucket.

resolve_allowed_buckets()'s other branch -- a claim that's *present but
empty* must deny outright, not fall back to the default scope -- isn't
covered here: a real Keycloak user-attribute mapper doesn't emit a claim
at all for an empty attribute, so "present but empty" and "absent" are
indistinguishable at the token level through a standard mapper. That
branch is proven directly in Rust instead (see
resolve_allowed_buckets_falls_back_to_default_only_when_claim_is_truly_absent
in server/src/s3/sts.rs), which constructs the literal {"buckets": []}
JSON a non-standard IdP could still plausibly send.

Setup is idempotent (a 409 from an already-existing realm/client/user/
mapper is treated as success), so this works for a fresh CI Keycloak
container and for a developer re-running it locally without tearing
anything down first.
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

SERVER_URL = os.environ.get("WARPDRIVE_SERVER_URL", "http://localhost:9710")

ALICE_BUCKET = os.environ.get("WARPDRIVE_STS_TEST_ALICE_BUCKET", "sts-tenant-alice")
BOB_BUCKET = os.environ.get("WARPDRIVE_STS_TEST_BOB_BUCKET", "sts-tenant-bob")
# Must differ from both of the above -- it's what proves the claim-absent
# fallback actually lands on the *default* scope, not accidentally on a
# real tenant's bucket.
DEFAULT_BUCKET = os.environ.get("WARPDRIVE_STS_TEST_DEFAULT_BUCKET", "sts-tenant-default")

# (username, password, email, buckets attribute or None for "omit the attribute entirely")
#
# No "claim present but empty" user here: tried it (an empty multivalued
# Keycloak user attribute), and Keycloak's standard attribute mapper doesn't
# emit the claim at all when the attribute has zero values -- "present but
# empty" and "absent" are indistinguishable at the token level through a
# real IdP's default mapper. That branch of resolve_allowed_buckets() (an
# explicit empty-array claim must deny, not fall back to the default scope)
# is instead proven directly and deterministically in
# server/src/s3/sts.rs's resolve_allowed_buckets_falls_back_to_default_only_when_claim_is_truly_absent
# unit test, which constructs the literal {"buckets": []} JSON.
TEST_USERS = [
    ("alice", "alicepass", "alice@example.com", [ALICE_BUCKET]),
    ("bob", "bobpass", "bob@example.com", [BOB_BUCKET]),
    ("carol", "carolpass", "carol@example.com", None),  # no attribute at all -> claim absent
]


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
    """Idempotent: creates the realm/client/mapper/users if they don't already exist."""
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

    # Keycloak's declarative User Profile silently drops any user attribute
    # not explicitly declared in the realm's profile schema (unmanaged
    # attributes are disabled by default) -- without this, the "buckets"
    # attribute set on each test user below would just vanish, and every
    # user would silently end up on the claim-absent fallback path.
    profile = requests.get(f"{KEYCLOAK_URL}/admin/realms/{REALM}/users/profile", headers=headers, timeout=10).json()
    if profile.get("unmanagedAttributePolicy") != "ENABLED":
        profile["unmanagedAttributePolicy"] = "ENABLED"
        requests.put(f"{KEYCLOAK_URL}/admin/realms/{REALM}/users/profile", headers=headers, json=profile, timeout=10).raise_for_status()

    client_internal_id = requests.get(
        f"{KEYCLOAK_URL}/admin/realms/{REALM}/clients", headers=headers,
        params={"clientId": CLIENT_ID}, timeout=10,
    ).json()[0]["id"]

    # Projects each user's own "buckets" user attribute into their token as
    # a real JSON array claim -- this is what lets different users actually
    # get different session scopes, rather than every token sharing one
    # server-wide default.
    existing_mappers = requests.get(
        f"{KEYCLOAK_URL}/admin/realms/{REALM}/clients/{client_internal_id}/protocol-mappers/models",
        headers=headers, timeout=10,
    ).json()
    if not any(m["name"] == "buckets-claim-mapper" for m in existing_mappers):
        r = requests.post(
            f"{KEYCLOAK_URL}/admin/realms/{REALM}/clients/{client_internal_id}/protocol-mappers/models",
            headers=headers,
            json={
                "name": "buckets-claim-mapper",
                "protocol": "openid-connect",
                "protocolMapper": "oidc-usermodel-attribute-mapper",
                "config": {
                    "user.attribute": "buckets",
                    "claim.name": "buckets",
                    "jsonType.label": "String",
                    "multivalued": "true",
                    "id.token.claim": "true",
                    "access.token.claim": "true",
                    "userinfo.token.claim": "true",
                },
            },
            timeout=10,
        )
        if r.status_code not in (201, 409):
            r.raise_for_status()

    for username, password, email, buckets in TEST_USERS:
        user_repr = {
            "username": username,
            "enabled": True,
            "emailVerified": True,
            "email": email,
            # email/firstName/lastName are required here -- Keycloak's default
            # User Profile config implicitly triggers a VERIFY_PROFILE required
            # action (surfaced as the cryptic "Account is not fully set up" /
            # invalid_grant on the password grant) if they're missing, even
            # with requiredActions explicitly set to [].
            "firstName": username.capitalize(),
            "lastName": "Test",
            "requiredActions": [],
            "credentials": [{"type": "password", "value": password, "temporary": False}],
        }
        if buckets is not None:
            user_repr["attributes"] = {"buckets": buckets}
        r = requests.post(
            f"{KEYCLOAK_URL}/admin/realms/{REALM}/users", headers=headers, json=user_repr, timeout=10,
        )
        if r.status_code == 201:
            continue
        if r.status_code != 409:
            r.raise_for_status()
        # Already exists from an earlier run of this script against the same
        # Keycloak container -- update it in place rather than silently
        # skipping, so a re-run always reflects this script's current idea
        # of each user's attributes/credentials, not whatever they were set
        # to the first time this realm was created.
        existing_id = requests.get(
            f"{KEYCLOAK_URL}/admin/realms/{REALM}/users", headers=headers,
            params={"username": username, "exact": "true"}, timeout=10,
        ).json()[0]["id"]
        requests.put(
            f"{KEYCLOAK_URL}/admin/realms/{REALM}/users/{existing_id}", headers=headers, json=user_repr, timeout=10,
        ).raise_for_status()
        requests.put(
            f"{KEYCLOAK_URL}/admin/realms/{REALM}/users/{existing_id}/reset-password", headers=headers,
            json={"type": "password", "value": password, "temporary": False}, timeout=10,
        ).raise_for_status()


def get_id_token(username, password):
    resp = requests.post(
        f"{KEYCLOAK_URL}/realms/{REALM}/protocol/openid-connect/token",
        data={
            "client_id": CLIENT_ID,
            "username": username,
            "password": password,
            "grant_type": "password",
            "scope": "openid",
        },
        timeout=10,
    )
    resp.raise_for_status()
    return resp.json()["id_token"]


def assume_role(id_token, session_name):
    """Returns (Credentials, SubjectFromWebIdentityToken) or raises ClientError."""
    sts = boto3.client(
        "sts", endpoint_url=f"{SERVER_URL}/sts",
        aws_access_key_id="unused", aws_secret_access_key="unused", region_name="us-east-1",
    )
    resp = sts.assume_role_with_web_identity(
        RoleArn="arn:aws:iam::000000000000:role/warpdrive-sts-test",
        RoleSessionName=session_name,
        WebIdentityToken=id_token,
    )
    return resp["Credentials"], resp.get("SubjectFromWebIdentityToken")


def s3_client_for(creds):
    return boto3.client(
        "s3", endpoint_url=f"{SERVER_URL}/s3",
        aws_access_key_id=creds["AccessKeyId"], aws_secret_access_key=creds["SecretAccessKey"],
        aws_session_token=creds["SessionToken"], region_name="us-east-1",
        config=botocore.config.Config(s3={"addressing_style": "path"}),
    )


def is_access_denied(e: botocore.exceptions.ClientError) -> bool:
    return e.response["Error"]["Code"] in ("AccessDenied", "403", "Forbidden")


# --- Test cases -------------------------------------------------------------

def test_tenant_round_trip(tokens):
    """Each ordinary tenant can fully operate within their own bucket."""
    for name, bucket in (("alice", ALICE_BUCKET), ("bob", BOB_BUCKET)):
        creds, _ = assume_role(tokens[name], f"{name}-round-trip")
        s3 = s3_client_for(creds)
        s3.create_bucket(Bucket=bucket)
        s3.put_object(Bucket=bucket, Key="hello.txt", Body=f"hello from {name}".encode())
        body = s3.get_object(Bucket=bucket, Key="hello.txt")["Body"].read()
        if body != f"hello from {name}".encode():
            return False
    return True


def test_cross_tenant_bucket_access_denied(tokens):
    """alice's session must not be able to touch bob's bucket, or vice versa."""
    alice_creds, _ = assume_role(tokens["alice"], "alice-cross-tenant")
    alice_s3 = s3_client_for(alice_creds)
    try:
        alice_s3.put_object(Bucket=BOB_BUCKET, Key="intrusion.txt", Body=b"should not land")
        return False  # must have been rejected
    except botocore.exceptions.ClientError as e:
        if not is_access_denied(e):
            return False

    bob_creds, _ = assume_role(tokens["bob"], "bob-cross-tenant")
    bob_s3 = s3_client_for(bob_creds)
    try:
        bob_s3.get_object(Bucket=ALICE_BUCKET, Key="hello.txt")
        return False
    except botocore.exceptions.ClientError as e:
        return is_access_denied(e)


def test_cross_tenant_copy_denied(tokens):
    """
    Live proof of the CopyObject/UploadPartCopy source-bucket-scope fix:
    bob puts a real object in his own bucket, then alice's session -- scoped
    only to her own bucket -- tries to CopyObject it in via
    x-amz-copy-source. Before the fix this silently succeeded (only the
    destination bucket was ever checked); it must now be denied.
    """
    bob_creds, _ = assume_role(tokens["bob"], "bob-seed-copy-source")
    bob_s3 = s3_client_for(bob_creds)
    bob_s3.put_object(Bucket=BOB_BUCKET, Key="secret.txt", Body=b"bob's data, not alice's")

    alice_creds, _ = assume_role(tokens["alice"], "alice-attempt-cross-copy")
    alice_s3 = s3_client_for(alice_creds)
    try:
        alice_s3.copy_object(
            Bucket=ALICE_BUCKET, Key="stolen.txt",
            CopySource={"Bucket": BOB_BUCKET, "Key": "secret.txt"},
        )
        return False  # must have been rejected
    except botocore.exceptions.ClientError as e:
        return is_access_denied(e)


def test_tenants_get_distinct_identities(tokens):
    """Two different users' sessions must not collapse into the same identity or credentials."""
    alice_creds, alice_subject = assume_role(tokens["alice"], "alice-identity-check")
    bob_creds, bob_subject = assume_role(tokens["bob"], "bob-identity-check")
    if not alice_subject or not bob_subject:
        return False
    if alice_subject == bob_subject:
        return False
    if alice_creds["AccessKeyId"] == bob_creds["AccessKeyId"]:
        return False
    return True


def test_claim_absent_falls_back_to_default_but_stays_isolated(tokens):
    """
    carol has no 'buckets' attribute at all -- resolve_allowed_buckets must
    fall back to WARPDRIVE_OIDC_DEFAULT_BUCKETS (not deny her outright), but
    that fallback scope must still not reach a real tenant's bucket.
    """
    creds, _ = assume_role(tokens["carol"], "carol-fallback")
    s3 = s3_client_for(creds)
    s3.create_bucket(Bucket=DEFAULT_BUCKET)
    s3.put_object(Bucket=DEFAULT_BUCKET, Key="hello.txt", Body=b"hello from carol via default scope")
    body = s3.get_object(Bucket=DEFAULT_BUCKET, Key="hello.txt")["Body"].read()
    if body != b"hello from carol via default scope":
        return False

    try:
        s3.get_object(Bucket=ALICE_BUCKET, Key="hello.txt")
        return False
    except botocore.exceptions.ClientError as e:
        return is_access_denied(e)


def test_tampered_token_rejected(tokens):
    tampered = tokens["alice"][:-5] + "AAAAA"
    try:
        assume_role(tampered, "tampered-token-test")
        return False
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
        s3.list_objects_v2(Bucket=ALICE_BUCKET)
        return False
    except botocore.exceptions.ClientError:
        return True


def main():
    print(f"Keycloak: {KEYCLOAK_URL}  WarpDrive: {SERVER_URL}")

    admin_token = get_admin_token()
    setup_keycloak(admin_token)

    tokens = {username: get_id_token(username, password) for username, password, _, _ in TEST_USERS}
    print(f"Minted real Keycloak id_tokens for: {', '.join(tokens)}")

    tests = [
        ("Each tenant round-trips in their own bucket", lambda: test_tenant_round_trip(tokens)),
        ("Cross-tenant bucket access denied (alice<->bob)", lambda: test_cross_tenant_bucket_access_denied(tokens)),
        ("Cross-tenant CopyObject source-bucket denied", lambda: test_cross_tenant_copy_denied(tokens)),
        ("Different tenants get distinct identities/credentials", lambda: test_tenants_get_distinct_identities(tokens)),
        ("Claim-absent falls back to default scope, still isolated", lambda: test_claim_absent_falls_back_to_default_but_stays_isolated(tokens)),
        ("Tampered WebIdentityToken rejected", lambda: test_tampered_token_rejected(tokens)),
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
