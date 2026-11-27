"""
Live cross-node proof of the cluster bucket registry: two distinct
Keycloak identities, each assuming their STS role against a *different*
cluster node, racing to claim the same bucket name. Proves the claim
replicates cluster-wide (not just locally on whichever node happened to
see it first) -- bob's own node (node1) must independently agree with
what alice's node (node0) decided, having never been told directly by
alice's own request.

Requires a running Keycloak (KEYCLOAK_URL, see
server/tests/python/sts_oidc_test.py for how to start one) and a local
5-node cluster with OIDC configured:

  export WARPDRIVE_OIDC_ISSUER="http://localhost:8080/realms/warpdrive"
  export WARPDRIVE_OIDC_AUDIENCE="warpdrive-client"
  export WARPDRIVE_OIDC_JWKS_URL="http://localhost:8080/realms/warpdrive/protocol/openid-connect/certs"
  export WARPDRIVE_OIDC_USER_CLAIM="sub"
  export WARPDRIVE_OIDC_BUCKETS_CLAIM="buckets"
  export WARPDRIVE_OIDC_DEFAULT_BUCKETS="cluster-default"
  ./scripts/run_local_cluster.sh

  python3 server/tests/python/bucket_registry_cluster_test.py
"""
import json
import sys
import boto3
import botocore
import requests

KEYCLOAK_URL = "http://localhost:8080"
REALM = "warpdrive"
CLIENT_ID = "warpdrive-client"
NODE0 = "http://127.0.0.1:9710"
NODE1 = "http://127.0.0.1:9711"
NODE2 = "http://127.0.0.1:9712"
SHARED_BUCKET = "cluster-registry-shared"


def admin_token():
    return requests.post(
        f"{KEYCLOAK_URL}/realms/master/protocol/openid-connect/token",
        data={"client_id": "admin-cli", "username": "admin", "password": "admin", "grant_type": "password"},
        timeout=10,
    ).json()["access_token"]


def setup_keycloak():
    headers = {"Authorization": f"Bearer {admin_token()}", "Content-Type": "application/json"}
    r = requests.post(f"{KEYCLOAK_URL}/admin/realms", headers=headers, json={"realm": REALM, "enabled": True}, timeout=10)
    assert r.status_code in (201, 409), r.text

    # Keycloak's declarative User Profile drops any undeclared user
    # attribute unless unmanaged attributes are allowed (same gotcha hit
    # building the Phase 10 multi-tenant tests).
    profile = requests.get(f"{KEYCLOAK_URL}/admin/realms/{REALM}/users/profile", headers=headers, timeout=10).json()
    if profile.get("unmanagedAttributePolicy") != "ENABLED":
        profile["unmanagedAttributePolicy"] = "ENABLED"
        requests.put(f"{KEYCLOAK_URL}/admin/realms/{REALM}/users/profile", headers=headers, json=profile, timeout=10).raise_for_status()

    r = requests.post(
        f"{KEYCLOAK_URL}/admin/realms/{REALM}/clients", headers=headers,
        json={"clientId": CLIENT_ID, "enabled": True, "publicClient": True, "directAccessGrantsEnabled": True, "standardFlowEnabled": False},
        timeout=10,
    )
    assert r.status_code in (201, 409), r.text
    client_internal_id = requests.get(
        f"{KEYCLOAK_URL}/admin/realms/{REALM}/clients", headers=headers, params={"clientId": CLIENT_ID}, timeout=10,
    ).json()[0]["id"]

    existing_mappers = requests.get(
        f"{KEYCLOAK_URL}/admin/realms/{REALM}/clients/{client_internal_id}/protocol-mappers/models", headers=headers, timeout=10,
    ).json()
    if not any(m["name"] == "buckets-claim-mapper" for m in existing_mappers):
        r = requests.post(
            f"{KEYCLOAK_URL}/admin/realms/{REALM}/clients/{client_internal_id}/protocol-mappers/models", headers=headers,
            json={
                "name": "buckets-claim-mapper", "protocol": "openid-connect", "protocolMapper": "oidc-usermodel-attribute-mapper",
                "config": {
                    "user.attribute": "buckets", "claim.name": "buckets", "jsonType.label": "String",
                    "multivalued": "true", "id.token.claim": "true", "access.token.claim": "true", "userinfo.token.claim": "true",
                },
            },
            timeout=10,
        )
        assert r.status_code in (201, 409), r.text

    for username, password in [("alice", "alicepass"), ("bob", "bobpass")]:
        user_repr = {
            "username": username, "enabled": True, "emailVerified": True,
            "email": f"{username}@example.com", "firstName": username.capitalize(), "lastName": "Test",
            "requiredActions": [], "credentials": [{"type": "password", "value": password, "temporary": False}],
            # Both alice and bob are ALLOWED (per their own IdP claims) to
            # attempt the shared bucket -- that's the point. Only one of
            # them actually OWNS it in WarpDrive's data model, which is
            # exactly the distinction this feature exists to enforce.
            "attributes": {"buckets": [SHARED_BUCKET]},
        }
        r = requests.post(f"{KEYCLOAK_URL}/admin/realms/{REALM}/users", headers=headers, json=user_repr, timeout=10)
        if r.status_code == 409:
            existing_id = requests.get(
                f"{KEYCLOAK_URL}/admin/realms/{REALM}/users", headers=headers, params={"username": username, "exact": "true"}, timeout=10,
            ).json()[0]["id"]
            requests.put(f"{KEYCLOAK_URL}/admin/realms/{REALM}/users/{existing_id}", headers=headers, json=user_repr, timeout=10).raise_for_status()
            requests.put(
                f"{KEYCLOAK_URL}/admin/realms/{REALM}/users/{existing_id}/reset-password", headers=headers,
                json={"type": "password", "value": password, "temporary": False}, timeout=10,
            ).raise_for_status()
        else:
            assert r.status_code == 201, r.text


def get_id_token(username, password):
    resp = requests.post(
        f"{KEYCLOAK_URL}/realms/{REALM}/protocol/openid-connect/token",
        data={"client_id": CLIENT_ID, "username": username, "password": password, "grant_type": "password", "scope": "openid"},
        timeout=10,
    )
    resp.raise_for_status()
    return resp.json()["id_token"]


def assume_role(node_url, id_token, session_name):
    sts = boto3.client("sts", endpoint_url=f"{node_url}/sts", aws_access_key_id="unused", aws_secret_access_key="unused", region_name="us-east-1")
    resp = sts.assume_role_with_web_identity(
        RoleArn="arn:aws:iam::000000000000:role/warpdrive-br-test",
        RoleSessionName=session_name,
        WebIdentityToken=id_token,
    )
    return resp["Credentials"]


def s3_client_for(node_url, creds):
    return boto3.client(
        "s3", endpoint_url=f"{node_url}/cluster/s3",
        aws_access_key_id=creds["AccessKeyId"], aws_secret_access_key=creds["SecretAccessKey"],
        aws_session_token=creds["SessionToken"], region_name="us-east-1",
        config=botocore.config.Config(s3={"addressing_style": "path"}),
    )


def main():
    print("Setting up Keycloak realm/users...")
    setup_keycloak()
    alice_token = get_id_token("alice", "alicepass")
    bob_token = get_id_token("bob", "bobpass")

    tests_passed = 0
    tests_total = 0

    def check(name, cond):
        nonlocal tests_passed, tests_total
        tests_total += 1
        print(f"--- {name} ---")
        print("PASS" if cond else "FAIL")
        if cond:
            tests_passed += 1

    # alice assumes her role against node0, claims the shared bucket there.
    print(f"\nalice: AssumeRoleWithWebIdentity against {NODE0}")
    alice_creds = assume_role(NODE0, alice_token, "alice-br-test")
    alice_s3 = s3_client_for(NODE0, alice_creds)
    alice_s3.put_object(Bucket=SHARED_BUCKET, Key="hello.txt", Body=b"alice was here first")
    check("alice's PUT via node0 claims the bucket", True)  # didn't raise

    # bob assumes HIS role against a DIFFERENT node (node1), and attempts
    # to write to the SAME bucket name there. node1 never talked to node0
    # about alice's claim directly -- it only knows because the claim
    # broadcast reached it independently when alice's node0 request fanned
    # out to all 5 peers.
    print(f"\nbob: AssumeRoleWithWebIdentity against {NODE1} (different node)")
    bob_creds = assume_role(NODE1, bob_token, "bob-br-test")
    bob_s3 = s3_client_for(NODE1, bob_creds)
    try:
        bob_s3.put_object(Bucket=SHARED_BUCKET, Key="intrusion.txt", Body=b"should not land")
        check("bob's cross-node claim attempt denied (owned by alice)", False)
    except botocore.exceptions.ClientError as e:
        check("bob's cross-node claim attempt denied (owned by alice)", e.response["Error"]["Code"] in ("AccessDenied", "403", "Forbidden"))

    # Confirm via a THIRD node (node2, which neither alice nor bob's
    # requests ever touched directly) that the registry agrees too --
    # proves this isn't just a node0/node1 pairwise thing, it's genuinely
    # cluster-wide.
    print(f"\nChecking from a third node ({NODE2}) that never saw either request directly")
    alice_creds_node2 = assume_role(NODE2, alice_token, "alice-br-test-node2")
    alice_s3_node2 = s3_client_for(NODE2, alice_creds_node2)
    body = alice_s3_node2.get_object(Bucket=SHARED_BUCKET, Key="hello.txt")["Body"].read()
    check("alice's object is visible and correct from a third node", body == b"alice was here first")

    bob_creds_node2 = assume_role(NODE2, bob_token, "bob-br-test-node2")
    bob_s3_node2 = s3_client_for(NODE2, bob_creds_node2)
    try:
        bob_s3_node2.put_object(Bucket=SHARED_BUCKET, Key="intrusion2.txt", Body=b"still should not land")
        check("bob denied from the third node too", False)
    except botocore.exceptions.ClientError as e:
        check("bob denied from the third node too", e.response["Error"]["Code"] in ("AccessDenied", "403", "Forbidden"))

    # Sanity: alice can still write more objects into her own bucket from
    # any node -- the gate doesn't block the legitimate owner anywhere.
    alice_s3_node2.put_object(Bucket=SHARED_BUCKET, Key="more.txt", Body=b"alice again, from node2")
    check("alice's own continued writes from a different node still work", True)

    # Bucket-policy storage (owner-only write, raw document round trip --
    # no Effect/Principal/Action evaluation, just the plumbing a later
    # RFC2.13 evaluator will read from).
    policy = json.dumps({"Version": "2012-10-17", "Statement": []})
    alice_s3.put_bucket_policy(Bucket=SHARED_BUCKET, Policy=policy)
    got = alice_s3_node2.get_bucket_policy(Bucket=SHARED_BUCKET)["Policy"]
    check("owner's policy write is visible from a different node", got == policy)

    try:
        bob_s3_node2.put_bucket_policy(Bucket=SHARED_BUCKET, Policy=policy)
        check("non-owner's policy write is denied", False)
    except botocore.exceptions.ClientError as e:
        check("non-owner's policy write is denied", e.response["Error"]["Code"] in ("AccessDenied", "403", "Forbidden"))

    print(f"\n{tests_passed}/{tests_total} cluster bucket registry tests passed")
    return 0 if tests_passed == tests_total else 1


if __name__ == "__main__":
    sys.exit(main())
