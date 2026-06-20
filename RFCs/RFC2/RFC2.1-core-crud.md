# RFC 2.1: Core Object CRUD — Fix What's Broken

**Status:** Complete — see [TEST-COVERAGE.md](../../TEST-COVERAGE.md) for verified passing tests.  
**Parent RFC:** [RFC 2 Overview](RFC2-s3-compatibility-plan.md)  
**Date:** June 2026

---

## Priority

Highest. Every S3 client hits these operations on every request. Nothing else in the test suite is meaningful until status codes, ETags, and error formats are correct.

## Schema Change

Add columns to the `haystack` table:

```sql
ALTER TABLE haystack ADD COLUMN etag TEXT;
ALTER TABLE haystack ADD COLUMN size INTEGER;
ALTER TABLE haystack ADD COLUMN content_type TEXT;
ALTER TABLE haystack ADD COLUMN last_modified TEXT;
ALTER TABLE haystack ADD COLUMN user_metadata TEXT;  -- JSON blob of x-amz-meta-* headers
ALTER TABLE objects ADD COLUMN cache_control TEXT;
ALTER TABLE objects ADD COLUMN expires TEXT;
```

## Changes Required

| Issue | Current Behavior | Required Behavior |
|---|---|---|
| ETag on PUT | `"s3-etag-placeholder"` | MD5 of body, hex-encoded, in double quotes |
| DELETE Object status | 200 or 404 | **204 always** — S3 delete is idempotent |
| HEAD Object | Always returns 200 with empty headers | Check existence; 404 if missing; return real `Content-Length`, `Content-Type`, `ETag`, `Last-Modified` |
| GET Object headers | Missing `Content-Type`, wrong `Content-Length` | Full headers from stored metadata |
| S3 error responses | Plain text body | `<Error><Code>...</Code><Message>...</Message><Resource>...</Resource><RequestId>...</RequestId></Error>` XML |
| CreateBucket | No-op, not persisted | Create bucket record in SQLite; return `200` with `Location: /{bucket}` |
| DeleteBucket | Route missing | `204` if bucket empty; `409 BucketNotEmpty` otherwise |
| Bucket existence | Not checked | All object operations return `404 NoSuchBucket` against a missing bucket |

## Ceph Tests Targeted

**test_s3.py (21):** `test_bucket_create_delete`, `test_object_write_read_update_read_delete`, `test_object_head_zero_bytes`, `test_object_write_check_etag`, `test_bucket_head`, `test_bucket_head_notexist`, `test_bucket_notexist`, `test_bucketv2_notexist`, `test_bucket_delete_notexist`, `test_bucket_delete_nonempty`, `test_object_read_not_exist`, `test_object_write_to_nonexist_bucket`, `test_buckets_create_then_list`, `test_buckets_list_ctime`, `test_object_write_cache_control`, `test_object_write_expires`, `test_object_write_file`, `test_object_requestid_matches_header_on_error`, `test_bucket_head_extended`, `test_object_delete_key_bucket_gone`, `test_object_read_unreadable`

**test_headers.py — 19 passing, 29 excluded (see notes below):**

**Passing (19):** `test_object_create_bad_md5_invalid_short`, `test_object_create_bad_md5_bad`, `test_object_create_bad_md5_empty`, `test_object_create_bad_md5_none`, `test_object_create_bad_expect_mismatch`, `test_object_create_bad_expect_empty`, `test_object_create_bad_expect_none`, `test_object_create_bad_contentlength_empty`, `test_object_create_bad_contentlength_negative`, `test_object_create_bad_contenttype_invalid`, `test_object_create_bad_contenttype_empty`, `test_object_create_bad_contenttype_none`, `test_bucket_create_contentlength_none`, `test_object_acl_create_contentlength_none`, `test_bucket_create_bad_expect_mismatch`, `test_bucket_create_bad_expect_empty`, `test_bucket_create_bad_contentlength_empty`, `test_bucket_create_bad_contentlength_negative`, `test_bucket_create_bad_contentlength_none`

**Not covered — legacy AWS Signature Version 2 (21 tests):** AWS SigV2 is deprecated (retired by AWS in 2023) and not implemented in Warpdrive. All `_aws2` tests use SigV2 signing and cannot pass without implementing the legacy signing protocol. These are intentionally excluded.

`test_object_create_bad_md5_invalid_garbage_aws2`, `test_object_create_bad_contentlength_mismatch_below_aws2`, `test_object_create_bad_authorization_incorrect_aws2`, `test_object_create_bad_authorization_invalid_aws2`, `test_object_create_bad_ua_empty_aws2`, `test_object_create_bad_ua_none_aws2`, `test_object_create_bad_date_invalid_aws2`, `test_object_create_bad_date_empty_aws2`, `test_object_create_bad_date_none_aws2`, `test_object_create_bad_date_before_today_aws2`, `test_object_create_bad_date_before_epoch_aws2`, `test_object_create_bad_date_after_end_aws2`, `test_bucket_create_bad_authorization_invalid_aws2`, `test_bucket_create_bad_ua_empty_aws2`, `test_bucket_create_bad_ua_none_aws2`, `test_bucket_create_bad_date_invalid_aws2`, `test_bucket_create_bad_date_empty_aws2`, `test_bucket_create_bad_date_none_aws2`, `test_bucket_create_bad_date_before_today_aws2`, `test_bucket_create_bad_date_after_today_aws2`, `test_bucket_create_bad_date_before_epoch_aws2`

**Not covered — boto3 test framework limitation (7 tests):** These tests attempt to forge invalid headers (empty/missing `Authorization`, malformed `X-Amz-Date`, missing `Content-Length`) by hooking `before-call`, but boto3's SigV4 signing runs *after* `before-call` and rewrites those headers before the request is sent. There is no server-side fix — the tests are unfixable in this form and are also marked `fails_on_rgw` (the Ceph reference S3 implementation fails them too).

`test_object_create_bad_contentlength_none`, `test_object_create_bad_authorization_empty`, `test_object_create_date_and_amz_date`, `test_object_create_amz_date_and_no_date`, `test_object_create_bad_authorization_none`, `test_bucket_create_bad_authorization_empty`, `test_bucket_create_bad_authorization_none`

**Not covered — deferred to RFC 2.10 (ACL) (1 test):** `test_bucket_put_bad_canned_acl` requires ACL endpoint validation (`PUT /{bucket}?acl`). Deferred to RFC 2.10.
