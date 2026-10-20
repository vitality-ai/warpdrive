#!/usr/bin/env python3
# Author: Tejus Chandrashekar
"""
POST Object (RFC 2.14, HTML form upload) test client.

Uses `requests` directly rather than boto3, since the Ceph `s3-tests`
suite's own `test_post_object_*` tests sign with the classic (SigV2-era)
POST policy scheme -- `AWSAccessKeyId` + `signature =
base64(HMAC-SHA1(secret_key, policy_base64))` -- which boto3 has no
client method for constructing directly, and several tests need to send
deliberately malformed signatures/policies that no SDK would let you build.

Every case here, including the exact error-code assertions, is taken
directly from the real Ceph `s3-tests` source
(`s3tests/functional/test_s3.py::test_post_object_*`), not guessed from
this project's own RFC2.14 doc -- that doc assumed a SigV4 form
(`x-amz-credential`/`x-amz-signature`) which the actual test suite does
not use.
"""

import base64
import datetime
import hashlib
import hmac
import json
import sys
import xml.etree.ElementTree as ET

import boto3
import requests
from botocore.client import Config

ACCESS_KEY = "AKIAIOSFODNN7EXAMPLE"
SECRET_KEY = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"
SERVER_URL = "http://localhost:9710"
S3_ENDPOINT_URL = f"{SERVER_URL}/s3"


def create_s3_client():
    return boto3.client(
        's3', endpoint_url=S3_ENDPOINT_URL,
        aws_access_key_id=ACCESS_KEY, aws_secret_access_key=SECRET_KEY,
        region_name='us-east-1',
        config=Config(signature_version='s3v4', s3={'addressing_style': 'path'}),
    )


def post_url(bucket: str) -> str:
    return f"{S3_ENDPOINT_URL}/{bucket}"


def sign(policy_b64: bytes) -> bytes:
    return base64.b64encode(hmac.new(SECRET_KEY.encode(), policy_b64, hashlib.sha1).digest())


def make_policy(conditions, expiration=None):
    if expiration is None:
        expiration = (datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(seconds=6000)).strftime("%Y-%m-%dT%H:%M:%SZ")
    return base64.b64encode(json.dumps({"expiration": expiration, "conditions": conditions}).encode())


def signed_payload(bucket, key="foo.txt", acl="private", content_type="text/plain", extra_conditions=None, extra_fields=None):
    conditions = [
        {"bucket": bucket},
        ["starts-with", "$key", "foo"],
        {"acl": acl},
    ]
    if content_type is not None:
        conditions.append(["starts-with", "$Content-Type", "text/plain"])
    conditions.append(["content-length-range", 0, 1024])
    if extra_conditions:
        conditions.extend(extra_conditions)
    policy = make_policy(conditions)
    payload = {
        "key": key, "AWSAccessKeyId": ACCESS_KEY, "acl": acl,
        "signature": sign(policy), "policy": policy,
    }
    if content_type is not None:
        payload["Content-Type"] = content_type
    if extra_fields:
        payload.update(extra_fields)
    return payload


def test_authenticated_request(s3):
    bucket = "post-test-auth"
    s3.create_bucket(Bucket=bucket)
    payload = signed_payload(bucket)
    r = requests.post(post_url(bucket), files={**payload, "file": ("bar", "bar")})
    if r.status_code != 204:
        print(f"expected 204, got {r.status_code}: {r.text[:200]}")
        return False
    body = s3.get_object(Bucket=bucket, Key="foo.txt")["Body"].read()
    return body == b"bar"


def test_bad_access_key(s3):
    bucket = "post-test-badkey"
    s3.create_bucket(Bucket=bucket)
    payload = {**signed_payload(bucket), "AWSAccessKeyId": "foo"}
    r = requests.post(post_url(bucket), files={**payload, "file": ("bar", "bar")})
    return r.status_code == 403


def test_invalid_signature(s3):
    bucket = "post-test-badsig"
    s3.create_bucket(Bucket=bucket)
    payload = signed_payload(bucket)
    payload["signature"] = payload["signature"][::-1]
    r = requests.post(post_url(bucket), files={**payload, "file": ("bar", "bar")})
    return r.status_code == 403


def test_missing_signature(s3):
    bucket = "post-test-nosig"
    s3.create_bucket(Bucket=bucket)
    payload = {k: v for k, v in signed_payload(bucket).items() if k != "signature"}
    r = requests.post(post_url(bucket), files={**payload, "file": ("bar", "bar")})
    return r.status_code == 400


def test_no_key_specified(s3):
    bucket = "post-test-nokey"
    s3.create_bucket(Bucket=bucket)
    payload = {k: v for k, v in signed_payload(bucket).items() if k != "key"}
    r = requests.post(post_url(bucket), files={**payload, "file": ("bar", "bar")})
    return r.status_code == 400


def test_invalid_date_format(s3):
    bucket = "post-test-baddate"
    s3.create_bucket(Bucket=bucket)
    policy = make_policy(
        [{"bucket": bucket}, ["starts-with", "$key", "foo"], {"acl": "private"},
         ["starts-with", "$Content-Type", "text/plain"], ["content-length-range", 0, 1024]],
        expiration=str(datetime.datetime.now(datetime.timezone.utc)),
    )
    payload = {"key": "foo.txt", "AWSAccessKeyId": ACCESS_KEY, "acl": "private",
               "signature": sign(policy), "policy": policy, "Content-Type": "text/plain"}
    r = requests.post(post_url(bucket), files={**payload, "file": ("bar", "bar")})
    return r.status_code == 400


def test_expires_is_case_sensitive(s3):
    bucket = "post-test-expirescase"
    s3.create_bucket(Bucket=bucket)
    expiration = (datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(seconds=6000)).strftime("%Y-%m-%dT%H:%M:%SZ")
    policy = base64.b64encode(json.dumps({
        "EXPIRATION": expiration,
        "conditions": [{"bucket": bucket}, ["starts-with", "$key", "foo"], {"acl": "private"},
                       ["starts-with", "$Content-Type", "text/plain"], ["content-length-range", 0, 1024]],
    }).encode())
    payload = {"key": "foo.txt", "AWSAccessKeyId": ACCESS_KEY, "acl": "private",
               "signature": sign(policy), "policy": policy, "Content-Type": "text/plain"}
    r = requests.post(post_url(bucket), files={**payload, "file": ("bar", "bar")})
    return r.status_code == 400


def test_expired_policy(s3):
    bucket = "post-test-expired"
    s3.create_bucket(Bucket=bucket)
    expired = (datetime.datetime.now(datetime.timezone.utc) - datetime.timedelta(seconds=6000)).strftime("%Y-%m-%dT%H:%M:%SZ")
    policy = make_policy(
        [{"bucket": bucket}, ["starts-with", "$key", "foo"], {"acl": "private"},
         ["starts-with", "$Content-Type", "text/plain"], ["content-length-range", 0, 1024]],
        expiration=expired,
    )
    payload = {"key": "foo.txt", "AWSAccessKeyId": ACCESS_KEY, "acl": "private",
               "signature": sign(policy), "policy": policy, "Content-Type": "text/plain"}
    r = requests.post(post_url(bucket), files={**payload, "file": ("bar", "bar")})
    return r.status_code == 403


def test_missing_conditions_list(s3):
    bucket = "post-test-noconds"
    s3.create_bucket(Bucket=bucket)
    expiration = (datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(seconds=6000)).strftime("%Y-%m-%dT%H:%M:%SZ")
    policy = base64.b64encode(json.dumps({"expiration": expiration}).encode())
    payload = {"key": "foo.txt", "AWSAccessKeyId": ACCESS_KEY, "acl": "private",
               "signature": sign(policy), "policy": policy, "Content-Type": "text/plain"}
    r = requests.post(post_url(bucket), files={**payload, "file": ("bar", "bar")})
    return r.status_code == 400


def test_empty_conditions(s3):
    bucket = "post-test-emptyconds"
    s3.create_bucket(Bucket=bucket)
    policy = make_policy([{}])
    payload = {"key": "foo.txt", "AWSAccessKeyId": ACCESS_KEY, "acl": "private",
               "signature": sign(policy), "policy": policy, "Content-Type": "text/plain"}
    r = requests.post(post_url(bucket), files={**payload, "file": ("bar", "bar")})
    return r.status_code == 400


def test_missing_policy_condition_no_bucket(s3):
    bucket = "post-test-nobucketcond"
    s3.create_bucket(Bucket=bucket)
    policy = make_policy([["starts-with", "$key", "foo"], {"acl": "private"},
                          ["starts-with", "$Content-Type", "text/plain"], ["content-length-range", 0, 1024]])
    payload = {"key": "foo.txt", "AWSAccessKeyId": ACCESS_KEY, "acl": "private",
               "signature": sign(policy), "policy": policy, "Content-Type": "text/plain"}
    r = requests.post(post_url(bucket), files={**payload, "file": ("bar", "bar")})
    return r.status_code == 403


def test_request_missing_policy_specified_field(s3):
    bucket = "post-test-missingfield"
    s3.create_bucket(Bucket=bucket)
    payload = signed_payload(bucket, extra_conditions=[["starts-with", "$x-amz-meta-foo", "bar"]])
    r = requests.post(post_url(bucket), files={**payload, "file": ("bar", "bar")})
    return r.status_code == 403


def test_user_specified_header(s3):
    bucket = "post-test-userheader"
    s3.create_bucket(Bucket=bucket)
    payload = signed_payload(bucket, extra_conditions=[["starts-with", "$x-amz-meta-foo", "bar"]],
                             extra_fields={"x-amz-meta-foo": "barclamp"})
    r = requests.post(post_url(bucket), files={**payload, "file": ("bar", "bar")})
    if r.status_code != 204:
        return False
    meta = s3.get_object(Bucket=bucket, Key="foo.txt")["Metadata"]
    return meta.get("foo") == "barclamp"


def test_invalid_request_field_value(s3):
    bucket = "post-test-badfieldvalue"
    s3.create_bucket(Bucket=bucket)
    payload = signed_payload(bucket, extra_conditions=[["eq", "$x-amz-meta-foo", ""]],
                             extra_fields={"x-amz-meta-foo": "barclamp"})
    r = requests.post(post_url(bucket), files={**payload, "file": ("bar", "bar")})
    return r.status_code == 403


def test_condition_is_case_sensitive(s3):
    bucket = "post-test-condcase"
    s3.create_bucket(Bucket=bucket)
    expiration = (datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(seconds=6000)).strftime("%Y-%m-%dT%H:%M:%SZ")
    policy = base64.b64encode(json.dumps({
        "expiration": expiration,
        "CONDITIONS": [{"bucket": bucket}, ["starts-with", "$key", "foo"], {"acl": "private"},
                       ["starts-with", "$Content-Type", "text/plain"], ["content-length-range", 0, 1024]],
    }).encode())
    payload = {"key": "foo.txt", "AWSAccessKeyId": ACCESS_KEY, "acl": "private",
               "signature": sign(policy), "policy": policy, "Content-Type": "text/plain"}
    r = requests.post(post_url(bucket), files={**payload, "file": ("bar", "bar")})
    return r.status_code == 400


def test_case_insensitive_condition_fields(s3):
    bucket = "post-test-fieldcase"
    s3.create_bucket(Bucket=bucket)
    policy = make_policy([{"bUcKeT": bucket}, ["StArTs-WiTh", "$KeY", "foo"], {"AcL": "private"},
                          ["StArTs-WiTh", "$CoNtEnT-TyPe", "text/plain"], ["content-length-range", 0, 1024]])
    payload = {"kEy": "foo.txt", "AWSAccessKeyId": ACCESS_KEY, "aCl": "private",
               "signature": sign(policy), "pOLICy": policy, "Content-Type": "text/plain"}
    r = requests.post(post_url(bucket), files={**payload, "file": ("bar", "bar")})
    return r.status_code == 204


def test_set_key_from_filename(s3):
    bucket = "post-test-filenamekey"
    s3.create_bucket(Bucket=bucket)
    policy = make_policy([{"bucket": bucket}, ["starts-with", "$key", "foo"], {"acl": "private"},
                          ["starts-with", "$Content-Type", "text/plain"], ["content-length-range", 0, 1024]])
    payload = {"key": "${filename}", "AWSAccessKeyId": ACCESS_KEY, "acl": "private",
               "signature": sign(policy), "policy": policy, "Content-Type": "text/plain"}
    r = requests.post(post_url(bucket), files={**payload, "file": ("foo.txt", "bar")})
    if r.status_code != 204:
        return False
    body = s3.get_object(Bucket=bucket, Key="foo.txt")["Body"].read()
    return body == b"bar"


def test_ignored_header(s3):
    bucket = "post-test-ignoredheader"
    s3.create_bucket(Bucket=bucket)
    payload = {**signed_payload(bucket), "x-ignore-foo": "bar"}
    r = requests.post(post_url(bucket), files={**payload, "file": ("bar", "bar")})
    return r.status_code == 204


def test_wrong_bucket(s3):
    bucket = "post-test-realbucket"
    bad_bucket = "post-test-wrongbucket"
    s3.create_bucket(Bucket=bucket)
    s3.create_bucket(Bucket=bad_bucket)
    policy = make_policy([{"bucket": bucket}, ["starts-with", "$key", "foo"], {"acl": "private"},
                          ["starts-with", "$Content-Type", "text/plain"], ["content-length-range", 0, 1024]])
    payload = {"key": "${filename}", "bucket": bucket, "AWSAccessKeyId": ACCESS_KEY, "acl": "private",
               "signature": sign(policy), "policy": policy, "Content-Type": "text/plain"}
    r = requests.post(post_url(bad_bucket), files={**payload, "file": ("foo.txt", "bar")})
    return r.status_code == 403


def test_upload_size_limit_exceeded(s3):
    bucket = "post-test-sizelimit"
    s3.create_bucket(Bucket=bucket)
    payload = signed_payload(bucket, extra_conditions=None)
    policy = make_policy([{"bucket": bucket}, ["starts-with", "$key", "foo"], {"acl": "private"},
                          ["starts-with", "$Content-Type", "text/plain"], ["content-length-range", 0, 0]])
    payload = {"key": "foo.txt", "AWSAccessKeyId": ACCESS_KEY, "acl": "private",
               "signature": sign(policy), "policy": policy, "Content-Type": "text/plain"}
    r = requests.post(post_url(bucket), files={**payload, "file": ("bar", "bar")})
    return r.status_code == 400


def test_upload_size_below_minimum(s3):
    bucket = "post-test-sizemin"
    s3.create_bucket(Bucket=bucket)
    policy = make_policy([{"bucket": bucket}, ["starts-with", "$key", "foo"], {"acl": "private"},
                          ["starts-with", "$Content-Type", "text/plain"], ["content-length-range", 512, 1000]])
    payload = {"key": "foo.txt", "AWSAccessKeyId": ACCESS_KEY, "acl": "private",
               "signature": sign(policy), "policy": policy, "Content-Type": "text/plain"}
    r = requests.post(post_url(bucket), files={**payload, "file": ("bar", "bar")})
    return r.status_code == 400


def test_missing_content_length_argument(s3):
    bucket = "post-test-missingclr"
    s3.create_bucket(Bucket=bucket)
    policy = make_policy([{"bucket": bucket}, ["starts-with", "$key", "foo"], {"acl": "private"},
                          ["starts-with", "$Content-Type", "text/plain"], ["content-length-range", 0]])
    payload = {"key": "foo.txt", "AWSAccessKeyId": ACCESS_KEY, "acl": "private",
               "signature": sign(policy), "policy": policy, "Content-Type": "text/plain"}
    r = requests.post(post_url(bucket), files={**payload, "file": ("bar", "bar")})
    return r.status_code == 400


def test_invalid_content_length_argument(s3):
    bucket = "post-test-badclr"
    s3.create_bucket(Bucket=bucket)
    policy = make_policy([{"bucket": bucket}, ["starts-with", "$key", "foo"], {"acl": "private"},
                          ["starts-with", "$Content-Type", "text/plain"], ["content-length-range", -1, 0]])
    payload = {"key": "foo.txt", "AWSAccessKeyId": ACCESS_KEY, "acl": "private",
               "signature": sign(policy), "policy": policy, "Content-Type": "text/plain"}
    r = requests.post(post_url(bucket), files={**payload, "file": ("bar", "bar")})
    return r.status_code == 400


def test_missing_expires_condition(s3):
    bucket = "post-test-noexpires"
    s3.create_bucket(Bucket=bucket)
    policy = base64.b64encode(json.dumps({
        "conditions": [{"bucket": bucket}, ["starts-with", "$key", "foo"], {"acl": "private"},
                       ["starts-with", "$Content-Type", "text/plain"], ["content-length-range", 0, 1024]],
    }).encode())
    payload = {"key": "foo.txt", "AWSAccessKeyId": ACCESS_KEY, "acl": "private",
               "signature": sign(policy), "policy": policy, "Content-Type": "text/plain"}
    r = requests.post(post_url(bucket), files={**payload, "file": ("bar", "bar")})
    return r.status_code == 400


def test_escaped_field_values(s3):
    bucket = "post-test-escaped"
    s3.create_bucket(Bucket=bucket)
    policy = make_policy([{"bucket": bucket}, ["starts-with", "$key", r"\$foo"], {"acl": "private"},
                          ["starts-with", "$Content-Type", "text/plain"], ["content-length-range", 0, 1024]])
    payload = {"key": r"\$foo.txt", "AWSAccessKeyId": ACCESS_KEY, "acl": "private",
               "signature": sign(policy), "policy": policy, "Content-Type": "text/plain"}
    r = requests.post(post_url(bucket), files={**payload, "file": ("bar", "bar")})
    if r.status_code != 204:
        return False
    body = s3.get_object(Bucket=bucket, Key=r"\$foo.txt")["Body"].read()
    return body == b"bar"


def test_authenticated_no_content_type(s3):
    bucket = "post-test-noct"
    s3.create_bucket(Bucket=bucket, ACL='public-read-write')
    policy = make_policy([{"bucket": bucket}, ["starts-with", "$key", "foo"], {"acl": "private"},
                          ["content-length-range", 0, 1024]])
    payload = {"key": "foo.txt", "AWSAccessKeyId": ACCESS_KEY, "acl": "private",
               "signature": sign(policy), "policy": policy}
    r = requests.post(post_url(bucket), files={**payload, "file": ("bar", "bar")})
    return r.status_code == 204


def test_success_redirect_action(s3):
    bucket = "post-test-redirect"
    s3.create_bucket(Bucket=bucket, ACL='public-read-write')
    redirect_url = post_url(bucket)
    policy = make_policy([{"bucket": bucket}, ["starts-with", "$key", "foo"], {"acl": "private"},
                          ["starts-with", "$Content-Type", "text/plain"],
                          ["eq", "$success_action_redirect", redirect_url],
                          ["content-length-range", 0, 1024]])
    payload = {"key": "foo.txt", "AWSAccessKeyId": ACCESS_KEY, "acl": "private",
               "signature": sign(policy), "policy": policy, "Content-Type": "text/plain",
               "success_action_redirect": redirect_url}
    r = requests.post(post_url(bucket), files={**payload, "file": ("bar", "bar")})
    if r.status_code != 200:
        print(f"expected final status 200 after following redirect, got {r.status_code}")
        return False
    etag = s3.get_object(Bucket=bucket, Key="foo.txt")["ETag"].strip('"')
    expected = f"{redirect_url}?bucket={bucket}&key=foo.txt&etag=%22{etag}%22"
    return r.url == expected


def test_set_success_code(s3):
    bucket = "post-test-successcode"
    s3.create_bucket(Bucket=bucket, ACL='public-read-write')
    payload = {"key": "foo.txt", "acl": "public-read", "success_action_status": "201", "Content-Type": "text/plain"}
    r = requests.post(post_url(bucket), files={**payload, "file": ("bar", "bar")})
    if r.status_code != 201:
        return False
    key_el = ET.fromstring(r.content).find('Key')
    return key_el is not None and key_el.text == "foo.txt"


def test_set_invalid_success_code(s3):
    bucket = "post-test-badsuccesscode"
    s3.create_bucket(Bucket=bucket, ACL='public-read-write')
    payload = {"key": "foo.txt", "acl": "public-read", "success_action_status": "404", "Content-Type": "text/plain"}
    r = requests.post(post_url(bucket), files={**payload, "file": ("bar", "bar")})
    return r.status_code == 204 and r.content.decode() == ""


def test_anonymous_request(s3):
    bucket = "post-test-anon"
    s3.create_bucket(Bucket=bucket, ACL='public-read-write')
    payload = {"key": "foo.txt", "acl": "public-read", "Content-Type": "text/plain"}
    r = requests.post(post_url(bucket), files={**payload, "file": ("bar", "bar")})
    if r.status_code != 204:
        return False
    body = s3.get_object(Bucket=bucket, Key="foo.txt")["Body"].read()
    return body == b"bar"


def test_tags_authenticated_request(s3):
    bucket = "post-test-tags"
    s3.create_bucket(Bucket=bucket)
    xml_tagset = "<Tagging><TagSet><Tag><Key>0</Key><Value>0</Value></Tag><Tag><Key>1</Key><Value>1</Value></Tag></TagSet></Tagging>"
    payload = signed_payload(bucket, extra_conditions=[["starts-with", "$tagging", ""]],
                             extra_fields={"tagging": xml_tagset})
    r = requests.post(post_url(bucket), files={**payload, "file": ("bar", "bar")})
    if r.status_code != 204:
        return False
    tags = s3.get_object_tagging(Bucket=bucket, Key="foo.txt")["TagSet"]
    return {t['Key']: t['Value'] for t in tags} == {'0': '0', '1': '1'}


def test_tags_anonymous_request(s3):
    bucket = "post-test-anontags"
    s3.create_bucket(Bucket=bucket, ACL='public-read-write')
    xml_tagset = "<Tagging><TagSet><Tag><Key>0</Key><Value>0</Value></Tag><Tag><Key>1</Key><Value>1</Value></Tag></TagSet></Tagging>"
    payload = {"key": "foo.txt", "acl": "public-read", "Content-Type": "text/plain", "tagging": xml_tagset}
    r = requests.post(post_url(bucket), files={**payload, "file": ("bar", "bar")})
    if r.status_code != 204:
        return False
    tags = s3.get_object_tagging(Bucket=bucket, Key="foo.txt")["TagSet"]
    return {t['Key']: t['Value'] for t in tags} == {'0': '0', '1': '1'}


def test_upload_checksum(s3):
    bucket = "post-test-checksum"
    s3.create_bucket(Bucket=bucket)
    megabytes = 1024 * 1024
    policy = make_policy([{"bucket": bucket}, ["starts-with", "$key", "foo_cksum_test"], {"acl": "private"},
                          ["starts-with", "$Content-Type", "text/plain"], ["content-length-range", 0, 5 * megabytes]])
    test_payload = b'x' * (2 * megabytes)
    checksum = base64.b64encode(hashlib.sha256(test_payload).digest()).decode()
    payload = {"key": "foo_cksum_test.txt", "AWSAccessKeyId": ACCESS_KEY, "acl": "private",
               "signature": sign(policy), "policy": policy, "Content-Type": "text/plain",
               "x-amz-checksum-sha256": checksum}
    r = requests.post(post_url(bucket), files={**payload, "file": test_payload})
    return r.status_code == 204


def test_upload_size_rgw_chunk_size_bug(s3):
    # https://tracker.ceph.com/issues/58627
    rgw_max_chunk_size = 4 * 2**20  # 4MiB
    min_size = rgw_max_chunk_size
    max_size = rgw_max_chunk_size * 3
    test_payload_size = rgw_max_chunk_size + 200  # push it just over the chunk boundary
    bucket = "post-test-chunkbug"
    s3.create_bucket(Bucket=bucket)
    policy = make_policy([{"bucket": bucket}, ["starts-with", "$key", "foo"], {"acl": "private"},
                          ["starts-with", "$Content-Type", "text/plain"], ["content-length-range", min_size, max_size]])
    test_payload = b'x' * test_payload_size
    payload = {"key": "foo.txt", "AWSAccessKeyId": ACCESS_KEY, "acl": "private",
               "signature": sign(policy), "policy": policy, "Content-Type": "text/plain"}
    r = requests.post(post_url(bucket), files={**payload, "file": test_payload})
    return r.status_code == 204


def test_upload_larger_than_chunk(s3):
    bucket = "post-test-largerchunk"
    s3.create_bucket(Bucket=bucket)
    policy = make_policy([{"bucket": bucket}, ["starts-with", "$key", "foo"], {"acl": "private"},
                          ["starts-with", "$Content-Type", "text/plain"], ["content-length-range", 0, 5 * 1024 * 1024]])
    foo_string = b'foo' * 1024 * 1024
    payload = {"key": "foo.txt", "AWSAccessKeyId": ACCESS_KEY, "acl": "private",
               "signature": sign(policy), "policy": policy, "Content-Type": "text/plain"}
    r = requests.post(post_url(bucket), files={**payload, "file": foo_string})
    if r.status_code != 204:
        return False
    body = s3.get_object(Bucket=bucket, Key="foo.txt")["Body"].read()
    return body == foo_string


def main():
    print("=== WarpDrive POST Object (RFC 2.14) Test Client ===")
    print(f"S3 Endpoint: {S3_ENDPOINT_URL}")
    print("=" * 60)

    s3 = create_s3_client()

    tests = [
        ("authenticated_request", test_authenticated_request),
        ("bad_access_key", test_bad_access_key),
        ("invalid_signature", test_invalid_signature),
        ("missing_signature", test_missing_signature),
        ("no_key_specified", test_no_key_specified),
        ("invalid_date_format", test_invalid_date_format),
        ("expires_is_case_sensitive", test_expires_is_case_sensitive),
        ("expired_policy", test_expired_policy),
        ("missing_conditions_list", test_missing_conditions_list),
        ("empty_conditions", test_empty_conditions),
        ("missing_policy_condition_no_bucket", test_missing_policy_condition_no_bucket),
        ("request_missing_policy_specified_field", test_request_missing_policy_specified_field),
        ("user_specified_header", test_user_specified_header),
        ("invalid_request_field_value", test_invalid_request_field_value),
        ("condition_is_case_sensitive", test_condition_is_case_sensitive),
        ("case_insensitive_condition_fields", test_case_insensitive_condition_fields),
        ("set_key_from_filename", test_set_key_from_filename),
        ("ignored_header", test_ignored_header),
        ("wrong_bucket", test_wrong_bucket),
        ("upload_size_limit_exceeded", test_upload_size_limit_exceeded),
        ("upload_size_below_minimum", test_upload_size_below_minimum),
        ("missing_content_length_argument", test_missing_content_length_argument),
        ("invalid_content_length_argument", test_invalid_content_length_argument),
        ("missing_expires_condition", test_missing_expires_condition),
        ("escaped_field_values", test_escaped_field_values),
        ("authenticated_no_content_type", test_authenticated_no_content_type),
        ("success_redirect_action", test_success_redirect_action),
        ("set_success_code", test_set_success_code),
        ("set_invalid_success_code", test_set_invalid_success_code),
        ("anonymous_request", test_anonymous_request),
        ("tags_authenticated_request", test_tags_authenticated_request),
        ("tags_anonymous_request", test_tags_anonymous_request),
        ("upload_checksum", test_upload_checksum),
        ("upload_size_rgw_chunk_size_bug", test_upload_size_rgw_chunk_size_bug),
        ("upload_larger_than_chunk", test_upload_larger_than_chunk),
    ]

    results = []
    for name, fn in tests:
        try:
            ok = fn(s3)
        except Exception as e:
            print(f"{name} raised an exception: {e}")
            ok = False
        results.append((name, ok))
        print(f"{name}: {'PASS' if ok else 'FAIL'}")

    passed = sum(1 for _, ok in results if ok)
    print("\n" + "=" * 60)
    print(f"Total: {passed}/{len(results)} passed")
    return 0 if passed == len(results) else 1


if __name__ == "__main__":
    sys.exit(main())
