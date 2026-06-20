# RFC 2: S3 Compatibility Plan for Warpdrive

**Status:** Draft  
**Date:** June 2026  
**Authors:** Tejas-ChandraShekarRaju

---

## Abstract

This RFC defines the phased plan for achieving S3 API compatibility in Warpdrive. Tests are sourced from the Ceph S3 test suite (`s3-tests`) and organized into batches by priority — starting with the operations used by virtually every S3 client and progressing toward advanced features. Each batch identifies what currently exists, what is broken or missing, and which Ceph tests it unlocks.

**Coverage goal:** all tests in `test_s3.py` (760) and `test_headers.py` (48), totalling 808 in-scope tests. The files `test_iam.py`, `test_sts.py`, `test_sns.py`, `test_s3select.py`, and `test_s3control.py` are explicitly out of scope (IAM/STS live in Vitality Console; SNS, S3 Select, and S3 Control are separate services).

---

## Background

Warpdrive is a custom object storage system built for disaggregated architectures. The server exposes an S3-compatible API at `/s3/...` backed by a local haystack storage engine and SQLite metadata store. The Ceph `s3-tests` suite is used as the S3 conformance benchmark.

### Current State Summary

| Area | Status |
|---|---|
| SigV4 authentication | Works, but **requires Vitality Console** (no standalone mode) |
| PUT / GET / DELETE Object | Partial — broken status codes, placeholder ETag, no metadata stored |
| HEAD Object | Stub — always returns 200 even if object doesn't exist |
| ListObjects | V2 only, returns `<Size>0</Size>`, no ETag or LastModified |
| CreateBucket | No-op — not persisted |
| DeleteBucket | Route missing |
| Bucket existence check | Not enforced |
| Error responses | Plain text — should be S3 XML |
| Multipart upload | Basic — part keys collide with real object keys, no state table |
| CopyObject | Has cross-bucket bug, placeholder ETag |
| Multi-object delete | Not implemented |
| Conditional requests | Not implemented |
| Range GET | Not implemented |
| Presigned URLs | Not implemented |
| CORS | Not implemented |
| Tagging | Not implemented |
| ACL | Not implemented |
| Versioning | Not implemented |

### IAM / Auth Scope

IAM lives in **Vitality Console**, not in Warpdrive. For the purpose of this plan, Warpdrive runs with a single hardcoded admin user (see Prerequisite below). Later, when a user explicitly configures and connects Vitality Console, non-admin credentials will be routed there.

---

## Prerequisite: Admin User Mode

**Must be implemented before any batch.** All s3-tests require a working S3 endpoint with credentials. Currently auth hard-requires `VITALITY_CONSOLE_URL` and `WARPDRIVE_SERVICE_SECRET`, making standalone testing impossible.

### Design

Add two new optional env vars:

```
WARPDRIVE_ADMIN_ACCESS_KEY=adminkey
WARPDRIVE_ADMIN_SECRET_KEY=adminsecret
```

In `src/s3/auth.rs`, before consulting Vitality Console, check whether the request's access key matches `WARPDRIVE_ADMIN_ACCESS_KEY`. If it does:

- Skip all Console network calls.
- Use `WARPDRIVE_ADMIN_SECRET_KEY` as the secret for SigV4 verification.
- Set `owner_id = "admin"`.
- Set `allowed_buckets = all` (query SQLite directly so list-buckets works without a pre-registered set).

The existing Console path remains unchanged. If neither admin key nor Console is configured, authentication fails as before.

**Future hook:** If Vitality Console is configured and the user explicitly invalidates the default admin user via the Console UI, the admin access key will be rejected locally and re-routed to Console on the next request.

---

## Batch 1: Core Object CRUD — Fix What's Broken

**Status:** In Progress — see [TEST-COVERAGE.md](../../TEST-COVERAGE.md) for which tests are verified passing.

**Priority:** Highest. Every S3 client hits these operations on every request. Nothing else in the test suite is meaningful until status codes, ETags, and error formats are correct.

### Schema Change

Add columns to the `haystack` table:

```sql
ALTER TABLE haystack ADD COLUMN etag TEXT;
ALTER TABLE haystack ADD COLUMN size INTEGER;
ALTER TABLE haystack ADD COLUMN content_type TEXT;
ALTER TABLE haystack ADD COLUMN last_modified TEXT;
ALTER TABLE haystack ADD COLUMN user_metadata TEXT;  -- JSON blob of x-amz-meta-* headers
```

### Changes Required

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

### Ceph Tests Targeted

**test_s3.py:** `test_bucket_create_delete`, `test_object_write_read_update_read_delete`, `test_object_head_zero_bytes`, `test_object_write_check_etag`, `test_bucket_head`, `test_bucket_head_notexist`, `test_bucket_notexist`, `test_bucketv2_notexist`, `test_bucket_delete_notexist`, `test_bucket_delete_nonempty`, `test_object_read_not_exist`, `test_object_write_to_nonexist_bucket`, `test_buckets_create_then_list`, `test_buckets_list_ctime`, `test_object_write_cache_control`, `test_object_write_expires`, `test_object_write_file`, `test_object_requestid_matches_header_on_error`, `test_bucket_head_extended`, `test_object_delete_key_bucket_gone`, `test_object_read_unreadable`

**test_headers.py — 19 passing, 29 excluded (see notes below):**

**Passing (19):** `test_object_create_bad_md5_invalid_short`, `test_object_create_bad_md5_bad`, `test_object_create_bad_md5_empty`, `test_object_create_bad_md5_none`, `test_object_create_bad_expect_mismatch`, `test_object_create_bad_expect_empty`, `test_object_create_bad_expect_none`, `test_object_create_bad_contentlength_empty`, `test_object_create_bad_contentlength_negative`, `test_object_create_bad_contenttype_invalid`, `test_object_create_bad_contenttype_empty`, `test_object_create_bad_contenttype_none`, `test_bucket_create_contentlength_none`, `test_object_acl_create_contentlength_none`, `test_bucket_create_bad_expect_mismatch`, `test_bucket_create_bad_expect_empty`, `test_bucket_create_bad_contentlength_empty`, `test_bucket_create_bad_contentlength_negative`, `test_bucket_create_bad_contentlength_none`

**Not covered — legacy AWS Signature Version 2 (21 tests):** AWS SigV2 is deprecated (retired by AWS in 2023) and not implemented in Warpdrive. All `_aws2` tests use SigV2 signing and cannot pass without implementing the legacy signing protocol. These are intentionally excluded.

`test_object_create_bad_md5_invalid_garbage_aws2`, `test_object_create_bad_contentlength_mismatch_below_aws2`, `test_object_create_bad_authorization_incorrect_aws2`, `test_object_create_bad_authorization_invalid_aws2`, `test_object_create_bad_ua_empty_aws2`, `test_object_create_bad_ua_none_aws2`, `test_object_create_bad_date_invalid_aws2`, `test_object_create_bad_date_empty_aws2`, `test_object_create_bad_date_none_aws2`, `test_object_create_bad_date_before_today_aws2`, `test_object_create_bad_date_before_epoch_aws2`, `test_object_create_bad_date_after_end_aws2`, `test_bucket_create_bad_authorization_invalid_aws2`, `test_bucket_create_bad_ua_empty_aws2`, `test_bucket_create_bad_ua_none_aws2`, `test_bucket_create_bad_date_invalid_aws2`, `test_bucket_create_bad_date_empty_aws2`, `test_bucket_create_bad_date_none_aws2`, `test_bucket_create_bad_date_before_today_aws2`, `test_bucket_create_bad_date_after_today_aws2`, `test_bucket_create_bad_date_before_epoch_aws2`

**Not covered — boto3 test framework limitation (7 tests):** These tests attempt to forge invalid headers (empty/missing `Authorization`, malformed `X-Amz-Date`, missing `Content-Length`) by hooking `before-call`, but boto3's SigV4 signing runs *after* `before-call` and rewrites those headers before the request is sent. There is no server-side fix — the tests are unfixable in this form and are also marked `fails_on_rgw` (the Ceph reference S3 implementation fails them too).

`test_object_create_bad_contentlength_none`, `test_object_create_bad_authorization_empty`, `test_object_create_date_and_amz_date`, `test_object_create_amz_date_and_no_date`, `test_object_create_bad_authorization_none`, `test_bucket_create_bad_authorization_empty`, `test_bucket_create_bad_authorization_none`

**Not covered — deferred to Batch 10 (ACL) (1 test):** `test_bucket_put_bad_canned_acl` requires ACL endpoint validation (`PUT /{bucket}?acl`). Deferred to Batch 10.

---

## Batch 2: Object Listing — Full ListObjects V1 + V2

**Priority:** High. The Ceph suite has ~65 listing tests. Listing is what separates a working object store from a toy. Current code only supports `list-type=2`, returns `<Size>0</Size>`, and lacks any filtering.

### Changes Required

- `GET /s3/{bucket}` without `list-type=2` → **ListObjectsV1** with `marker`-based pagination
- `GET /s3/{bucket}?list-type=2` → **ListObjectsV2** with `continuation-token`, `start-after`, `fetch-owner`
- **Prefix filtering:** `prefix=foo/` returns only keys with that prefix
- **Delimiter + CommonPrefixes:** `delimiter=/` collapses key segments into virtual directories returned as `<CommonPrefixes><Prefix>...</Prefix></CommonPrefixes>`
- **MaxKeys + truncation:** default 1000; return `<IsTruncated>true</IsTruncated>` and next marker/token when limit is hit
- **`encoding-type=url`:** URL-encode all keys and prefixes in the XML response
- Real `<Size>`, `<ETag>`, `<LastModified>` per `<Contents>` entry (from new metadata columns in Batch 1)
- `<KeyCount>` in V2 response
- `GET /s3` (list all buckets) with `?max-buckets=N` pagination support

### Ceph Tests Targeted

All ~65 `test_bucket_list_*` and `test_bucket_listv2_*` tests, including: empty, distinct, many, delimiter variants, prefix variants, maxkeys variants, marker/continuation-token variants, encoding, unordered, fetchowner, `test_basic_key_count`, `test_bucket_list_return_data`, `test_bucket_list_return_data_versioning`, `test_bucket_list_objects_anonymous*`, `test_list_buckets_paginated`

---

## Batch 3: Object Properties & Conditional Requests

**Priority:** High. User metadata and conditional headers are required for real application patterns — caching, atomic updates, CMS workflows, and any client that uses ETags for consistency.

### Changes Required

**User metadata (x-amz-meta-*):**
- On PUT: capture all `x-amz-meta-*` request headers; serialize to JSON; store in the `user_metadata` column
- On GET / HEAD: deserialize and echo back as `x-amz-meta-*` response headers
- On PUT-overwrite: replace metadata entirely (not merge)
- `test_object_metadata_replaced_on_put` verifies this

**Content-Type:**
- Store the `Content-Type` request header on PUT (fall back to `application/octet-stream` if missing)
- Return it on GET and HEAD

**Conditional GET** (`If-Match`, `If-None-Match`, `If-Modified-Since`, `If-Unmodified-Since`):
- `If-Match: "etag"` — return 412 if stored ETag doesn't match; 200 if it does
- `If-None-Match: "etag"` — return 304 if stored ETag matches; 200 if it doesn't
- `If-Modified-Since: date` — return 304 if object hasn't changed since date
- `If-Unmodified-Since: date` — return 412 if object has changed since date

**Conditional PUT** (`If-Match`, `If-None-Match`):
- `If-Match: "etag"` — only overwrite if stored ETag matches; 412 otherwise
- `If-None-Match: *` — only write if object does not already exist; 412 if it does
- These also apply at CompleteMultipartUpload time (multipart conditional PUT)

**Conditional COPY** (`x-amz-copy-source-if-*`):
- `x-amz-copy-source-if-match: "etag"` — copy only if source ETag matches; 412 otherwise
- `x-amz-copy-source-if-none-match: "etag"` — copy only if source ETag does not match; 412 otherwise
- `x-amz-copy-source-if-modified-since: date` — copy only if source changed after date
- `x-amz-copy-source-if-unmodified-since: date` — copy only if source unchanged since date

**Conditional DELETE** (`If-Match`, `If-Modified-Since`, `If-Unmodified-Since`, size):
- `If-Match: "etag"` — delete only if stored ETag matches; 412 otherwise
- `If-Match: "etag"` with version ID — applies to specific version
- `If-Match: "size"` — delete only if object size matches (Ceph extension)
- Also applies to multi-object delete operations

### Ceph Tests Targeted

`test_object_set_get_metadata_none_to_good`, `test_object_set_get_metadata_none_to_empty`, `test_object_set_get_metadata_overwrite_to_empty`, `test_object_set_get_unicode_metadata`, `test_object_metadata_replaced_on_put`, `test_get_object_ifmatch_good`, `test_get_object_ifmatch_failed`, `test_get_object_ifnonematch_good`, `test_get_object_ifnonematch_failed`, `test_get_object_ifmodifiedsince_good`, `test_get_object_ifmodifiedsince_failed`, `test_get_object_ifunmodifiedsince_good`, `test_get_object_ifunmodifiedsince_failed`, `test_put_object_ifmatch_good`, `test_put_object_ifmatch_failed`, `test_put_object_ifmatch_overwrite_existed_good`, `test_put_object_ifmatch_nonexisted_failed`, `test_put_object_ifnonmatch_good`, `test_put_object_ifnonmatch_failed`, `test_put_object_ifnonmatch_nonexisted_good`, `test_put_object_ifnonmatch_overwrite_existed_failed`, `test_put_object_if_match`, `test_put_object_current_if_match`, `test_put_current_object_if_match`, `test_put_current_object_if_none_match`, `test_multipart_put_object_if_match`, `test_multipart_put_current_object_if_match`, `test_multipart_put_current_object_if_none_match`, `test_copy_object_ifmatch_good`, `test_copy_object_ifmatch_failed`, `test_copy_object_ifnonematch_good`, `test_copy_object_ifnonematch_failed`, `test_delete_object_if_match`, `test_delete_object_if_match_last_modified_time`, `test_delete_object_if_match_size`, `test_delete_object_current_if_match`, `test_delete_object_current_if_match_last_modified_time`, `test_delete_object_current_if_match_size`, `test_delete_object_version_if_match`, `test_delete_object_version_if_match_last_modified_time`, `test_delete_object_version_if_match_size`, `test_delete_objects_if_match`, `test_delete_objects_if_match_last_modified_time`, `test_delete_objects_if_match_size`, `test_delete_objects_current_if_match`, `test_delete_objects_current_if_match_last_modified_time`, `test_delete_objects_current_if_match_size`, `test_delete_objects_version_if_match`, `test_delete_objects_version_if_match_last_modified_time`, `test_delete_objects_version_if_match_size`

---

## Batch 4: Range Requests & Transfer Encoding

**Priority:** Medium-High. Range GET is critical for large-file resumable downloads, video streaming, and is required for multipart object GET-by-part (Batch 6). `100-continue` and `aws-chunked` are what the AWS CLI and SDK emit by default.

### Changes Required

**Range GET:**
- Parse `Range: bytes=start-end` header
- Map the byte range across the extent list stored in SQLite (warpdrive's disaggregated offset-size model makes this natural — slice the extents to cover `[start, end]`)
- Return `206 Partial Content` with `Content-Range: bytes start-end/total` and `Content-Length: (end-start+1)`
- Return `416 Range Not Satisfiable` for invalid ranges (start > end, start > object size)
- Handle suffix range (`bytes=-N` — last N bytes) and skip-leading (`bytes=N-`)
- Handle empty object range request (return 416)
- `read-through` behavior: no partial byte serving on a zero-length body

**100-Continue:**
- Handle `Expect: 100-continue` header correctly — actix-web handles most of this, but the handler must not reject the header or stall

**aws-chunked transfer encoding:**
- Detect `Content-Encoding: aws-chunked` or `x-amz-content-sha256: STREAMING-AWS4-HMAC-SHA256-PAYLOAD`
- Decode the chunked body format (each chunk is prefixed with `{hex-size};chunk-signature=...\r\n`) before passing bytes to storage

### Ceph Tests Targeted

`test_100_continue`, `test_100_continue_error_retry`, `test_object_content_encoding_aws_chunked`, `test_object_write_with_chunked_transfer_encoding`, `test_ranged_request_response_code`, `test_ranged_request_invalid_range`, `test_ranged_request_empty_object`, `test_ranged_request_skip_leading_bytes_response_code`, `test_ranged_request_return_trailing_bytes_response_code`, `test_ranged_big_request_response_code`, `test_read_through`, range-based subtests within `test_multipart_get_part`

---

## Batch 5: Multi-Object Delete & CopyObject Correctness

**Priority:** Medium-High. Multi-object delete is used by every S3 client for cleanup operations (e.g., `aws s3 rm --recursive`). CopyObject has correctness bugs that prevent cross-bucket operations from working.

### Changes Required

**Multi-Object Delete (`POST /s3/{bucket}?delete`):**
- Parse XML body: `<Delete><Object><Key>k1</Key></Object><Object><Key>k2</Key></Object></Delete>`
- Delete each key; collect successes and errors
- Return XML: `<DeleteResult><Deleted><Key>k1</Key></Deleted>...</DeleteResult>`
- Quiet mode: `<Delete><Quiet>true</Quiet>...` — only return errors, not successes
- Route: add `DELETE /s3/{bucket}` with `?delete` query param check
- Enforce max 1000 keys per request (both V1 and V2 variants)

**CopyObject fixes:**
- Fix source bucket parsing: `x-amz-copy-source` is `/source-bucket/source-key` (URL path with leading slash, percent-encoded) — current code uses `splitn(2, '/')` and gets it wrong
- Cross-bucket copy: source bucket may differ from destination bucket; use separate DB contexts for source read vs. destination write
- `x-amz-metadata-directive: COPY` (default) — copy source metadata to destination
- `x-amz-metadata-directive: REPLACE` — use new headers from the COPY request as metadata
- Real ETag on copy result (MD5 of copied data, not placeholder)
- Handle copies from buckets not owned by the requesting user (cross-owner) — check read permission on source
- Percent-encoded keys in `x-amz-copy-source` must be URL-decoded before lookup
- CopyObject source conditionals (`x-amz-copy-source-if-*`) handled here (logic shared with Batch 3 implementation)
- Multipart copy invalid/improper range handling: return `400 InvalidRange` for bad `x-amz-copy-source-range`

### Ceph Tests Targeted

`test_multi_object_delete`, `test_multi_objectv2_delete`, `test_multi_object_delete_key_limit`, `test_multi_objectv2_delete_key_limit`, `test_expected_bucket_owner`, `test_object_copy_zero_size`, `test_object_copy_16m`, `test_object_copy_same_bucket`, `test_object_copy_diff_bucket`, `test_object_copy_verify_contenttype`, `test_object_copy_to_itself`, `test_object_copy_to_itself_with_metadata`, `test_object_copy_retaining_metadata`, `test_object_copy_replacing_metadata`, `test_object_copy_bucket_not_found`, `test_object_copy_key_not_found`, `test_object_copy_not_owned_bucket`, `test_object_copy_not_owned_object_bucket`, `test_upload_part_copy_percent_encoded_key`, `test_multipart_copy_improper_range`, `test_multipart_copy_invalid_range`

---

## Batch 6: Multipart Upload — Full Correctness

**Priority:** Medium. Required for any object over 5GB and preferred for large objects. The current implementation stores parts as fake keys (`{key}.part.{n}.{uploadId}`) which collide with real object keys and don't track ETag per part.

### Schema Changes

Add two new tables:

```sql
CREATE TABLE multipart_uploads (
    upload_id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL,
    bucket TEXT NOT NULL,
    key TEXT NOT NULL,
    content_type TEXT,
    user_metadata TEXT,
    initiated_at TEXT NOT NULL
);

CREATE TABLE multipart_parts (
    upload_id TEXT NOT NULL,
    part_number INTEGER NOT NULL,
    etag TEXT NOT NULL,
    size INTEGER NOT NULL,
    offset_size_list BLOB NOT NULL,
    PRIMARY KEY (upload_id, part_number),
    FOREIGN KEY (upload_id) REFERENCES multipart_uploads(upload_id)
);
```

### Changes Required

- **CreateMultipartUpload:** insert row into `multipart_uploads`; return `UploadId`
- **UploadPart:** validate `uploadId` exists; store part data + ETag in `multipart_parts`; enforce 5MB minimum per part (except last)
- **CompleteMultipartUpload:** parse XML part list; validate ETags match stored values; validate part numbers are sequential from 1; concatenate extent lists in order; write final object metadata; clean up `multipart_uploads` and `multipart_parts` rows
- **AbortMultipartUpload:** delete all parts from `multipart_parts` and their raw storage extents; delete `multipart_uploads` row
- **ListMultipartUploads** (`GET /s3/{bucket}?uploads`): list in-progress uploads with prefix/delimiter/max-uploads filtering
- **GET by part number** (`GET /s3/{bucket}/{key}?partNumber=N`): return the bytes for that part of a completed multipart object
- **GetObjectAttributes** (`GET /s3/{bucket}/{key}?attributes`): return structured metadata including `ETag`, `StorageClass`, `ObjectSize`, and `ObjectParts` list for multipart objects
- Remove the old part-as-fake-key logic entirely

### Ceph Tests Targeted

`test_multipart_upload_empty`, `test_multipart_upload_complete_without_create`, `test_multipart_upload_small`, `test_multipart_upload`, `test_multipart_upload_multiple_sizes`, `test_multipart_upload_resend_part`, `test_multipart_upload_contents`, `test_multipart_upload_overwrite_existing_object`, `test_multipart_upload_size_too_small`, `test_multipart_upload_missing_part`, `test_multipart_upload_incorrect_etag`, `test_abort_multipart_upload`, `test_abort_multipart_upload_not_found`, `test_list_multipart_upload`, `test_list_multipart_upload_owner`, `test_multipart_get_part`, `test_multipart_single_get_part`, `test_non_multipart_get_part`, `test_multipart_copy_small`, `test_multipart_copy_multiple_sizes`, `test_multipart_copy_without_range`, `test_multipart_copy_special_names`, `test_atomic_multipart_upload_write`, `test_multipart_resend_first_finishes_last`, `test_get_multipart_object_attributes`, `test_get_paginated_multipart_object_attributes`, `test_get_single_multipart_object_attributes`

---

## Batch 7: Presigned URLs

**Priority:** Medium. Required for browser direct-upload flows, temporary access grants, and any use case where S3 credentials cannot be embedded in the client.

### Design

Presigned URLs move the SigV4 auth from the `Authorization` header into query parameters. No new storage is needed — this is pure auth logic in `src/s3/auth.rs`.

### Changes Required

- Detect presigned request: check for `X-Amz-Algorithm` query param (instead of `Authorization` header)
- Parse `X-Amz-Credential`, `X-Amz-Date`, `X-Amz-Expires`, `X-Amz-SignedHeaders`, `X-Amz-Signature` from query string
- Verify expiry: reject if `X-Amz-Date` (parsed as timestamp) + `X-Amz-Expires` (seconds) < current time; return `403 RequestExpired`
- Run the same SigV4 canonical request computation with query-string params instead of header params
- Allow anonymous bucket/object access for correctly-signed presigned GETs (bypass ACL check for the request's specific resource)
- Support SigV2 presigned format (query params `AWSAccessKeyId`, `Expires`, `Signature`) — several `_aws2` tests exercise this path
- Tenant-qualified access keys (key format `tenant$user`) must be resolved against both admin bypass and Vitality Console lookup
- `X-Amz-Algorithm=AWS4-HMAC-SHA256` (V4) and plain V2 presigned URLs must both work

### Ceph Tests Targeted

`test_object_raw_get`, `test_object_raw_get_bucket_gone`, `test_object_raw_get_object_gone`, `test_object_raw_authenticated`, `test_object_raw_authenticated_bucket_gone`, `test_object_raw_authenticated_object_gone`, `test_object_raw_get_x_amz_expires_not_expired`, `test_object_raw_get_x_amz_expires_out_range_zero`, `test_object_raw_get_x_amz_expires_out_max_range`, `test_object_raw_get_x_amz_expires_out_positive_range`, `test_object_raw_put_authenticated_expired`, `test_object_presigned_put_object_with_acl`, `test_object_raw_response_headers`, `test_object_raw_get_x_amz_expires_not_expired_tenant`, `test_cors_presigned_get_object_v2`, `test_cors_presigned_put_object_v2`, `test_cors_presigned_get_object_tenant`, `test_cors_presigned_get_object_tenant_v2`, `test_cors_presigned_put_object_tenant`, `test_cors_presigned_put_object_tenant_v2`, `test_cors_presigned_put_object_tenant_with_acl`

---

## Batch 8: Bucket Location & CORS

**Priority:** Medium. CORS is required for any browser-based S3 client or web application that accesses warpdrive directly from a browser. Location is cheap.

### Schema Change

```sql
CREATE TABLE bucket_cors (
    bucket TEXT PRIMARY KEY,
    cors_xml TEXT NOT NULL
);
```

### Changes Required

**Bucket Location:**
- `GET /s3/{bucket}?location` → `<LocationConstraint>us-east-1</LocationConstraint>` (or read from a `WARPDRIVE_REGION` env var, default `us-east-1`)

**CORS:**
- `PUT /s3/{bucket}?cors` — store XML body in `bucket_cors`
- `GET /s3/{bucket}?cors` — return stored XML; `404 NoSuchCORSConfiguration` if not set
- `DELETE /s3/{bucket}?cors` — remove row
- `OPTIONS /s3/{bucket}/{key}` — preflight: match `Origin` and `Access-Control-Request-Method` against stored rules; return `Access-Control-Allow-Origin`, `Access-Control-Allow-Methods`, `Access-Control-Allow-Headers`, `Access-Control-Max-Age`; `403` if no rule matches

### Ceph Tests Targeted

`test_bucket_get_location`, `test_set_cors`, `test_cors_origin_response`, `test_cors_origin_wildcard`, `test_cors_header_option`, `test_cors_presigned_get_object`, `test_cors_presigned_put_object`

---

## Batch 9: Tagging

**Priority:** Medium-Low. Tagging is used for cost allocation, lifecycle rule targeting, and workflow automation. Implementation is straightforward key-value storage.

### Schema Change

```sql
CREATE TABLE bucket_tags (
    bucket TEXT NOT NULL,
    tag_key TEXT NOT NULL,
    tag_value TEXT NOT NULL,
    PRIMARY KEY (bucket, tag_key)
);

CREATE TABLE object_tags (
    user_id TEXT NOT NULL,
    bucket TEXT NOT NULL,
    key TEXT NOT NULL,
    tag_key TEXT NOT NULL,
    tag_value TEXT NOT NULL,
    PRIMARY KEY (user_id, bucket, key, tag_key)
);
```

### Changes Required

- `PUT /s3/{bucket}?tagging` — replace all bucket tags with XML body `<Tagging><TagSet><Tag><Key>k</Key><Value>v</Value></Tag></TagSet></Tagging>`
- `GET /s3/{bucket}?tagging` — return tags XML; `404 NoSuchTagSet` if no tags set
- `DELETE /s3/{bucket}?tagging` — remove all tags
- Same three routes for `PUT/GET/DELETE /s3/{bucket}/{key}?tagging`
- `GET /s3/{bucket}/{key}` — include `x-amz-tagging-count` header when object has tags
- Enforce tag limits: max 10 tags per bucket/object; key max 128 chars; value max 256 chars; return `400 BadRequest` on excess
- `PUT /s3/{bucket}/{key}?tagging` with `x-amz-tagging` header on PUT — store inline tags
- On multipart uploads: support `x-amz-tagging` on CreateMultipartUpload
- Anonymous access respects ACL: public-read objects allow tag reads (gated by ACL batch)

### Ceph Tests Targeted

`test_set_bucket_tagging`, `test_get_obj_tagging`, `test_get_obj_head_tagging`, `test_put_delete_tags`, `test_put_excess_key_tags`, `test_put_excess_tags`, `test_put_excess_val_tags`, `test_put_max_kvsize_tags`, `test_put_max_tags`, `test_put_modify_tags`, `test_put_obj_with_tags`, `test_set_multipart_tagging`, `test_delete_tags_obj_public`, `test_get_tags_acl_public`, `test_put_tags_acl_public`, and all bucket/object tagging variant tests

---

## Batch 10: ACL — Canned ACLs, Block Public Access

**Priority:** Medium-Low. With a single admin user, full per-user ACL evaluation is not needed yet. Implement canned ACLs to support public-read buckets/objects and anonymous access patterns. Also implement Block Public Access controls.

### Design

ACL is hardcoded for the admin user: admin always has full access to everything they own. ACL only gates unauthenticated (anonymous) access to bucket/object resources. No per-user grant table needed at this stage.

### Schema Change

```sql
ALTER TABLE haystack ADD COLUMN acl TEXT DEFAULT 'private';

CREATE TABLE bucket_acl (
    bucket TEXT PRIMARY KEY,
    acl TEXT NOT NULL DEFAULT 'private'
);

CREATE TABLE public_access_block (
    bucket TEXT PRIMARY KEY,
    block_public_acls BOOLEAN NOT NULL DEFAULT false,
    ignore_public_acls BOOLEAN NOT NULL DEFAULT false,
    block_public_policy BOOLEAN NOT NULL DEFAULT false,
    restrict_public_buckets BOOLEAN NOT NULL DEFAULT false
);
```

### Changes Required

- On PUT (object or bucket): parse `x-amz-acl` header; store canned ACL value (`private`, `public-read`, `public-read-write`, `authenticated-read`)
- On anonymous GET (no `Authorization` header): check stored ACL; allow if `public-read` or `public-read-write`; return `403 AccessDenied` if `private`
- `PUT /s3/{bucket}?acl` — update bucket ACL
- `GET /s3/{bucket}?acl` — return bucket ACL as XML `<AccessControlPolicy>` with owner and grant list
- `PUT /s3/{bucket}/{key}?acl` — update object ACL
- `GET /s3/{bucket}/{key}?acl` — return object ACL XML
- Concurrent ACL set: `x-amz-acl` header on bucket PUT and `PUT ?acl` must be consistent under concurrent operations
- Header-based grant ACLs: `x-amz-grant-read`, `x-amz-grant-write`, `x-amz-grant-read-acp`, `x-amz-grant-full-control` headers (group grants including `AllUsers` URI)
- Object ACL timestamp: `Last-Modified` on object must update when ACL changes
- List-buckets anonymous: `403` (S3 never allows anonymous list-all-my-buckets)
- **Block Public Access** (`PUT/GET/DELETE /s3/{bucket}?publicAccessBlock`): store four flags per bucket; when `blockPublicAcls=true` reject public-read/write canned ACLs on PUT; when `ignorePublicAcls=true` treat all ACLs as private on read; when `blockPublicPolicy=true` reject bucket policy that grants public access; when `restrictPublicBuckets=true` block all public and cross-account access
- `GET /s3/{bucket}?policyStatus` — return `<PolicyStatus><IsPublic>...</IsPublic></PolicyStatus>`

### Ceph Tests Targeted

`test_object_anon_put`, `test_object_anon_put_write_access`, `test_object_put_authenticated`, `test_access_bucket_private_object_private`, `test_access_bucket_private_object_publicread`, `test_access_bucket_private_object_publicreadwrite`, `test_access_bucket_private_objectv2_private`, `test_access_bucket_private_objectv2_publicread`, `test_access_bucket_private_objectv2_publicreadwrite`, `test_access_bucket_publicread_object_private`, `test_access_bucket_publicread_object_publicread`, `test_access_bucket_publicread_object_publicreadwrite`, `test_access_bucket_publicreadwrite_*`, `test_bucket_acl_*`, `test_object_acl_*`, `test_list_buckets_anonymous`, `test_list_buckets_invalid_auth`, `test_list_buckets_bad_auth`, `test_bucket_concurrent_set_canned_acl`, `test_bucket_header_acl_grants`, `test_bucket_recreate_new_acl`, `test_bucket_recreate_overwrite_acl`, `test_object_copy_canned_acl`, `test_object_header_acl_grants`, `test_object_put_acl_mtime`, `test_object_raw_authenticated_bucket_acl`, `test_object_raw_authenticated_object_acl`, `test_object_raw_get_bucket_acl`, `test_object_raw_get_object_acl`, `test_put_bucket_acl_grant_group_read`, `test_object_presigned_put_object_with_acl_tenant`, `test_cors_presigned_put_object_with_acl`, `test_put_get_delete_public_block`, `test_put_public_block`, `test_get_undefined_public_block`, `test_block_public_object_canned_acls`, `test_block_public_put_bucket_acls`, `test_block_public_restrict_public_buckets`, `test_ignore_public_acls`, `test_get_authpublic_acl_bucket_policy_status`, `test_get_nonpublicpolicy_acl_bucket_policy_status`, `test_get_public_acl_bucket_policy_status`, `test_get_publicpolicy_acl_bucket_policy_status`, `test_get_public_block_deny_bucket_policy`

---

## Batch 11: Versioning

**Priority:** Low. Significant schema change. Every PUT creates a new version row rather than overwriting. Required for strong consistency guarantees and point-in-time recovery.

### Changes Required

- New `versioning_state` column per bucket (`disabled`, `enabled`, `suspended`)
- Versioning-aware object table: all writes produce a new version with a UUID `VersionId`; reads without version ID return the latest
- Delete with versioning enabled creates a delete marker (no data deleted)
- `GET /s3/{bucket}/{key}?versionId=...` — retrieve specific version
- `DELETE /s3/{bucket}/{key}?versionId=...` — permanently delete a specific version
- `GET /s3/{bucket}?versions` — ListObjectVersions with marker, prefix, delimiter pagination
- `PUT /s3/{bucket}?versioning` — enable/suspend versioning
- Multi-object delete with versioning: delete markers created per key; `VersionId` in response
- CopyObject with versioning: `x-amz-copy-source-version-id` to copy a specific version; destination receives new VersionId
- Multipart upload returns VersionId in CompleteMultipartUpload response when versioning is enabled
- Atomic upload: PUT to a versioned bucket must return VersionId immediately, visible on GET with no race
- Concurrent operations: concurrent PUTs to the same key each produce a distinct VersionId; concurrent deletes each produce a distinct delete-marker VersionId
- Versioned ACL: `GET/PUT ?acl` on a versioned object must work without specifying a versionId (operates on latest)
- Special key names with versioning: keys that look like version IDs or have spaces/special chars still work
- Suspended versioning: new writes get `VersionId=null`; null-version object treated as the "current" version
- Delete markers on non-versioned bucket: `DELETE` on a key that never existed returns 204 with no delete marker
- Delete marker expiration (Lifecycle rule `ExpiredObjectDeleteMarker` — implemented in Batch 20, but the marker creation is here)

### Ceph Tests Targeted

`test_versioning_bucket_create_suspend`, `test_versioning_obj_create_read_remove`, `test_versioning_obj_create_read_remove_head`, `test_versioning_stack_delete_merkers`, `test_versioning_obj_plain_null_version_*`, `test_versioning_obj_suspend_versions`, `test_versioning_obj_create_versions_remove_all`, `test_versioning_concurrent_multi_object_delete`, `test_versioning_multi_object_delete`, `test_versioning_multi_object_delete_with_marker`, `test_versioning_multi_object_delete_with_marker_create`, `test_versioning_copy_obj_version`, `test_versioning_obj_list_marker`, `test_versioning_obj_create_overwrite_multipart`, `test_versioning_obj_create_versions_remove_special_names`, `test_versioning_obj_suspended_copy`, `test_versioning_bucket_atomic_upload_return_version_id`, `test_versioning_bucket_multipart_upload_return_version_id`, `test_versioned_concurrent_object_create_and_remove`, `test_versioned_concurrent_object_create_concurrent_remove`, `test_versioned_object_acl`, `test_versioned_object_acl_no_version_specified`, `test_object_copy_versioned_bucket`, `test_object_copy_versioned_url_encoding`, `test_object_copy_versioning_multipart_upload`, `test_multipart_copy_versioned`, `test_delete_marker_nonversioned`, `test_delete_marker_versioned`, `test_delete_marker_expiration`, `test_delete_marker_suspended`, `test_get_versioned_object_attributes`

---

## Batch 12: Bucket Naming Validation & Ownership Controls

**Priority:** Medium-Low. Bucket naming rules enforce DNS compatibility. Ownership controls (BucketOwnerEnforced, BucketOwnerPreferred, ObjectWriter) determine which user becomes the object owner on cross-account writes.

### Schema Change

```sql
ALTER TABLE buckets ADD COLUMN object_ownership TEXT NOT NULL DEFAULT 'ObjectWriter';
```

### Changes Required

**Bucket naming validation** (enforced at `PUT /s3/{bucket}` — CreateBucket):
- Reject names shorter than 3 chars or longer than 63 chars → `400 InvalidBucketName`
- Reject names that are valid IPs (`192.168.1.1`) → `400 InvalidBucketName`
- Reject names starting with a digit-only segment or non-alphanumeric → `400 InvalidBucketName`
- Reject names with `..`, `-.`, `.-` sequences → `400 InvalidBucketName`
- Reject names ending with `-` → `400 InvalidBucketName`
- Accept names with hyphens, periods, and digits in valid positions
- `test_bucket_create_exists`: re-creating an existing bucket you own returns `200` (not an error)
- `test_bucket_create_exists_nonowner`: re-creating a bucket owned by a different user returns `409 BucketAlreadyExists`

**Ownership Controls** (`PUT/GET/DELETE /s3/{bucket}?ownershipControls`):
- Store `ObjectOwnership` setting per bucket (`BucketOwnerEnforced`, `BucketOwnerPreferred`, `ObjectWriter`)
- `BucketOwnerEnforced`: disables ACLs entirely; any `x-amz-acl` or grant header → `400 InvalidBucketAclWithObjectOwnership`
- `BucketOwnerPreferred`: ACLs still work; if uploader specifies `bucket-owner-full-control`, ownership transfers to bucket owner
- `ObjectWriter`: default; object owned by uploader
- `test_bucket_create_delete_bucket_ownership`: create and delete a bucket while checking ownership controls persist correctly

**Account usage & bucket stats:**
- `GET /s3/{bucket}?usage` — return `x-rgw-bytes-used` and `x-rgw-object-count` headers (Ceph extension)
- `HEAD /s3/{bucket}?read-stats=true` — return read statistics headers (Ceph extension)
- `GET /s3` — list-all-my-buckets supports `?max-buckets=N` for paginated response

### Ceph Tests Targeted

`test_bucket_create_naming_bad_ip`, `test_bucket_create_naming_bad_short_one`, `test_bucket_create_naming_bad_short_two`, `test_bucket_create_naming_bad_starts_nonalpha`, `test_bucket_create_naming_dns_dash_at_end`, `test_bucket_create_naming_dns_dash_dot`, `test_bucket_create_naming_dns_dot_dash`, `test_bucket_create_naming_dns_dot_dot`, `test_bucket_create_naming_dns_long`, `test_bucket_create_naming_dns_underscore`, `test_bucket_create_naming_good_contains_hyphen`, `test_bucket_create_naming_good_contains_period`, `test_bucket_create_naming_good_long_60`, `test_bucket_create_naming_good_long_61`, `test_bucket_create_naming_good_long_62`, `test_bucket_create_naming_good_long_63`, `test_bucket_create_naming_good_starts_alpha`, `test_bucket_create_naming_good_starts_digit`, `test_bucket_create_special_key_names`, `test_bucket_create_exists`, `test_bucket_create_exists_nonowner`, `test_bucket_recreate_not_overriding`, `test_bucket_create_delete_bucket_ownership`, `test_create_bucket_bucket_owner_enforced`, `test_create_bucket_bucket_owner_preferred`, `test_create_bucket_no_ownership_controls`, `test_create_bucket_object_writer`, `test_put_bucket_ownership_bucket_owner_enforced`, `test_put_bucket_ownership_bucket_owner_preferred`, `test_put_bucket_ownership_object_writer`, `test_account_usage`, `test_head_bucket_usage`, `test_list_buckets_paginated`

---

## Batch 13: Bucket Policy

**Priority:** Medium. Bucket policies allow fine-grained access control via JSON IAM-style policy documents. With a single admin user, the priority is lower, but several Ceph tests require basic policy support (allow/deny principals, condition operators, policy status).

### Schema Change

```sql
CREATE TABLE bucket_policy (
    bucket TEXT PRIMARY KEY,
    policy_json TEXT NOT NULL
);
```

### Design

Policy evaluation order: explicit Deny > explicit Allow > implicit Deny. For the single admin user, admin always has implicit Allow on their own buckets regardless of policy. Policy evaluation applies primarily to anonymous requests and (when Vitality Console is connected) non-admin users.

### Changes Required

- `PUT /s3/{bucket}?policy` — store JSON policy document; validate JSON structure; `400 MalformedPolicy` on bad JSON
- `GET /s3/{bucket}?policy` — return stored policy; `404 NoSuchBucketPolicy` if not set
- `DELETE /s3/{bucket}?policy` — remove policy
- Policy evaluation engine: parse `Effect`, `Principal`, `Action`, `Resource`, `Condition` fields
- Supported condition operators: `StringEquals`, `StringLike`, `StringNotEquals`, `ArnLike`, `IpAddress`, `Bool`, `*IfExists` variants (e.g., `StringEqualsIfExists`)
- `NotPrincipal` support: allow all except listed principals
- Policy-gated operations: `s3:GetObject`, `s3:PutObject`, `s3:DeleteObject`, `s3:ListBucket`, `s3:GetBucketAcl`, `s3:PutBucketAcl`, `s3:GetObjectTagging`, `s3:PutObjectTagging`, `s3:PutObjectAcl`, `s3:GetObjectAcl`, `s3:PutBucketPolicy`, `s3:GetBucketPolicy`, `s3:DeleteBucketPolicy`, `s3:ListBucketMultipartUploads`, `s3:AbortMultipartUpload`
- Tag-condition policies: `s3:RequestObjectTag`, `s3:ExistingObjectTag` condition keys
- Bucket policy status: `GET /s3/{bucket}?policyStatus` — whether the policy makes the bucket public (supplement to ACL-based status in Batch 10)
- Cross-account policy: deny access from a different account's principal
- Deny self policy: a policy that denies the bucket owner's own access must be rejectable via Console (prevent lockout)
- `HEAD /s3/{bucket}/{key}` returns `403` with `x-amz-request-id` when policy denies access (prefix condition)
- Multipart upload policy: policy conditions checked on `s3:PutObject` at CompleteMultipartUpload time
- Upload-part-copy policy: `s3:GetObject` on source evaluated at part-copy time

### Ceph Tests Targeted

`test_set_get_del_bucket_policy`, `test_bucket_policy`, `test_bucket_policy_acl`, `test_bucket_policy_allow_notprincipal`, `test_bucket_policy_another_bucket`, `test_bucket_policy_deny_self_denied_policy`, `test_bucket_policy_deny_self_denied_policy_confirm_header`, `test_bucket_policy_different_tenant`, `test_bucket_policy_get_obj_acl_existing_tag`, `test_bucket_policy_get_obj_existing_tag`, `test_bucket_policy_get_obj_tagging_existing_tag`, `test_bucket_policy_multipart`, `test_bucket_policy_put_obj_acl`, `test_bucket_policy_put_obj_copy_source`, `test_bucket_policy_put_obj_copy_source_meta`, `test_bucket_policy_put_obj_grant`, `test_bucket_policy_put_obj_request_obj_tag`, `test_bucket_policy_put_obj_tagging_existing_tag`, `test_bucket_policy_set_condition_operator_end_with_IfExists`, `test_bucket_policy_tenanted_bucket`, `test_bucket_policy_upload_part_copy`, `test_bucketv2_policy`, `test_bucketv2_policy_acl`, `test_bucketv2_policy_another_bucket`, `test_get_nonpublicpolicy_principal_bucket_policy_status`, `test_head_object_404_with_policy_prefix`, `test_multipart_upload_on_a_bucket_with_policy`, `test_block_public_policy`, `test_block_public_policy_with_principal`, `test_get_bucket_policy_status`, `test_get_nonpublicpolicy_acl_bucket_policy_status`, `test_post_object_expired_policy`, `test_post_object_missing_policy_condition`, `test_post_object_request_missing_policy_specified_field`

---

## Batch 14: POST Object (HTML Form Upload)

**Priority:** Medium-Low. POST-based upload allows browser-native file upload via HTML form without exposing credentials. It uses a policy document signed by the server to authorize specific form fields.

### Design

`POST /s3/{bucket}` with `multipart/form-data` encoding. The form contains a `key`, `policy` (base64 JSON), `x-amz-signature`, and optional fields. The server validates the signature and policy, then stores the object.

### Changes Required

- Route: `POST /s3/{bucket}` — parse `multipart/form-data` body
- Extract `key`, `Content-Type`, `policy`, `x-amz-algorithm`, `x-amz-credential`, `x-amz-date`, `x-amz-signature`, `x-amz-meta-*`, `tagging`, `acl`, `success_action_redirect`, `success_action_status` fields
- Policy validation: base64-decode → JSON parse `{"expiration": "...", "conditions": [...]}`; check `expiration` not past; match each condition against the form fields
- Condition types: `["eq", "$field", "value"]`, `["starts-with", "$field", "prefix"]`, `["content-length-range", min, max]`; conditions are case-insensitive on field names
- Signature verification: HMAC-SHA256 over the raw base64 policy string using SigV4 signing key
- On success: return `204` (or `200`/redirect per `success_action_status`/`success_action_redirect`)
- On error: return S3 XML error (not HTML) — `400 InvalidArgument`, `403 SignatureDoesNotMatch`, `403 ExpiredToken`
- Object size limits: `content-length-range` condition enforced; `413 EntityTooLarge` on excess
- Tags: `tagging` form field accepted as URL-encoded `key=value&key2=value2` pairs
- Anonymous POST: no signature — allowed if bucket policy or ACL grants public-write
- Checksum: `x-amz-checksum-*` in form field → verify and store

### Ceph Tests Targeted

`test_post_object_anonymous_request`, `test_post_object_authenticated_no_content_type`, `test_post_object_authenticated_request`, `test_post_object_authenticated_request_bad_access_key`, `test_post_object_case_insensitive_condition_fields`, `test_post_object_condition_is_case_sensitive`, `test_post_object_empty_conditions`, `test_post_object_escaped_field_values`, `test_post_object_expires_is_case_sensitive`, `test_post_object_ignored_header`, `test_post_object_invalid_access_key`, `test_post_object_invalid_content_length_argument`, `test_post_object_invalid_date_format`, `test_post_object_invalid_request_field_value`, `test_post_object_invalid_signature`, `test_post_object_missing_conditions_list`, `test_post_object_missing_content_length_argument`, `test_post_object_missing_expires_condition`, `test_post_object_missing_signature`, `test_post_object_no_key_specified`, `test_post_object_set_invalid_success_code`, `test_post_object_set_key_from_filename`, `test_post_object_set_success_code`, `test_post_object_success_redirect_action`, `test_post_object_tags_anonymous_request`, `test_post_object_tags_authenticated_request`, `test_post_object_upload_checksum`, `test_post_object_upload_larger_than_chunk`, `test_post_object_upload_size_below_minimum`, `test_post_object_upload_size_limit_exceeded`, `test_post_object_upload_size_rgw_chunk_size_bug`, `test_post_object_user_specified_header`, `test_post_object_wrong_bucket`

---

## Batch 15: Server-Side Encryption — SSE-S3

**Priority:** Medium-Low. SSE-S3 uses server-managed AES-256 keys transparent to the client. The client requests encryption with `x-amz-server-side-encryption: AES256`; the server encrypts before writing and decrypts on read. For Warpdrive's initial implementation, the "encryption" can be a passthrough with correct header echoing, with real AES added later when a key management approach is chosen.

### Design

Phase 1 (this batch): enforce correct request/response header handling and reject conflicting encryption headers. Actual AES-256 encryption of the stored bytes is optional in Phase 1 — the tests primarily check that the API surface is correct.

### Changes Required

- Accept `x-amz-server-side-encryption: AES256` on PUT; echo `x-amz-server-side-encryption: AES256` on GET/HEAD
- Store `sse_type` in metadata columns (`sse-s3`, `sse-kms`, `sse-c`, or null)
- Default bucket encryption: `PUT /s3/{bucket}?encryption` — store default SSE-S3 or SSE-KMS configuration; apply to all new objects that don't specify encryption explicitly
- `GET /s3/{bucket}?encryption` — return encryption configuration
- `DELETE /s3/{bucket}?encryption` — remove default encryption
- Reject `x-amz-server-side-encryption: aws:kms` with an invalid algorithm header → `400 InvalidArgument`
- Reject conflicting headers (e.g., SSE-C `x-amz-server-side-encryption-customer-*` with SSE-S3 header simultaneously)
- Multipart upload with SSE-S3: all parts must use the same SSE type; CompleteMultipartUpload returns correct SSE header

### Ceph Tests Targeted

`test_sse_s3_default_upload_1b`, `test_sse_s3_default_upload_1kb`, `test_sse_s3_default_upload_1mb`, `test_sse_s3_default_upload_8mb`, `test_sse_s3_encrypted_upload_1b`, `test_sse_s3_encrypted_upload_1kb`, `test_sse_s3_encrypted_upload_1mb`, `test_sse_s3_encrypted_upload_8mb`, `test_sse_s3_default_method_head`, `test_sse_s3_default_multipart_upload`, `test_sse_s3_default_post_object_authenticated_request`, `test_bucket_policy_put_obj_s3_incorrect_algo_sse_s3`, `test_put_bucket_encryption_s3`, `test_get_bucket_encryption_s3`, `test_delete_bucket_encryption_s3`

---

## Batch 16: Server-Side Encryption — SSE-C (Customer-Provided Keys)

**Priority:** Medium-Low. SSE-C lets the client supply the AES-256 key on every request. The server encrypts with that key and never stores it — the client must re-supply the same key on GET/HEAD/CopyObject or receive `403`.

### Changes Required

- Detect `x-amz-server-side-encryption-customer-algorithm: AES256` on PUT/GET/HEAD
- Extract key from `x-amz-server-side-encryption-customer-key` (base64-encoded 32 bytes)
- Extract MD5 from `x-amz-server-side-encryption-customer-key-MD5`; validate it matches the key's MD5; `400 InvalidArgument` if mismatch
- `400 InvalidArgument` if algorithm present but key missing, or key present but algorithm missing
- Encrypt object data with AES-256-CBC (or CTR) before writing to haystack; store IV alongside the extent
- On GET/HEAD: require same three headers; decrypt using supplied key; `403 AccessDenied` if key doesn't match stored HMAC
- `x-amz-server-side-encryption-customer-key` must NOT be echoed in response (only algorithm and key-MD5 are echoed)
- CopyObject with SSE-C source: `x-amz-copy-source-server-side-encryption-customer-*` headers for the source key; destination key is separate (can differ)
- Multipart SSE-C: each UploadPart must supply the same customer key; GetObjectAttributes returns SSE-C info
- Non-multipart GET-by-part (`?partNumber=N`) with SSE-C: supply key on GET
- Bucket policy: policy can enforce SSE-C (`s3:x-amz-server-side-encryption-customer-algorithm` condition key)
- `405 MethodNotAllowed` if SSE-C key present on an object that was stored without SSE-C (or with a different key)
- Unaligned multipart parts (last part smaller than 5MB but not the only part) still work with SSE-C
- POST object with SSE-C: `x-amz-server-side-encryption-customer-*` form fields

### Ceph Tests Targeted

`test_encryption_sse_c_present`, `test_encryption_sse_c_no_key`, `test_encryption_sse_c_no_md5`, `test_encryption_sse_c_invalid_md5`, `test_encryption_sse_c_other_key`, `test_encryption_key_no_sse_c`, `test_encryption_sse_c_method_head`, `test_encryption_sse_c_multipart_upload`, `test_encryption_sse_c_multipart_bad_download`, `test_encryption_sse_c_multipart_invalid_chunks_1`, `test_encryption_sse_c_multipart_invalid_chunks_2`, `test_encryption_sse_c_unaligned_multipart_upload`, `test_encryption_sse_c_post_object_authenticated_request`, `test_encryption_sse_c_enforced_with_bucket_policy`, `test_encryption_sse_c_deny_algo_with_bucket_policy`, `test_encrypted_transfer_1b`, `test_encrypted_transfer_1kb`, `test_encrypted_transfer_1MB`, `test_encrypted_transfer_13b`, `test_get_sse_c_encrypted_object_attributes`, `test_multipart_sse_c_get_part`, `test_non_multipart_sse_c_get_part`

---

## Batch 17: Server-Side Encryption — SSE-KMS

**Priority:** Low. SSE-KMS uses an AWS KMS key (or compatible KMS) to envelope-encrypt the data key. For Warpdrive, this requires either integrating with a real KMS endpoint or running a local KMS mock (e.g., localstack). The tests require `[s3 kms]` config section with a valid key ID.

### Design

Warpdrive defers to an external KMS endpoint configured via `WARPDRIVE_KMS_ENDPOINT` and `WARPDRIVE_KMS_KEY_ID`. If the env vars are absent, SSE-KMS requests return `400 InvalidArgument`.

### Changes Required

- Accept `x-amz-server-side-encryption: aws:kms` and optional `x-amz-server-side-encryption-aws-kms-key-id` on PUT
- Call KMS `GenerateDataKey` to obtain a data key; encrypt object with data key; store the encrypted data key blob alongside the extent
- Echo `x-amz-server-side-encryption: aws:kms` and `x-amz-server-side-encryption-aws-kms-key-id` on GET/HEAD
- On GET: call KMS `Decrypt` to recover the data key; decrypt object
- Default bucket KMS encryption: `PUT /s3/{bucket}?encryption` with `SSEAlgorithm: aws:kms` and `KMSMasterKeyID`
- `GET/DELETE /s3/{bucket}?encryption` for KMS config
- Conflict headers: reject `x-amz-server-side-encryption: aws:kms` combined with `x-amz-server-side-encryption-customer-*` → `400`
- Reject unknown algorithm in `x-amz-server-side-encryption` header → `400 InvalidArgument`
- Multipart KMS: all parts encrypted with same data key; correct headers returned
- KMS with bucket policy: policy condition `s3:x-amz-server-side-encryption` and `s3:x-amz-server-side-encryption-aws-kms-key-id`
- Copy with KMS: source and destination can have different KMS keys
- POST object with KMS: `x-amz-server-side-encryption` form field

### Ceph Tests Targeted

`test_sse_kms_present`, `test_sse_kms_no_key`, `test_sse_kms_not_declared`, `test_sse_kms_method_head`, `test_sse_kms_read_declare`, `test_sse_kms_transfer_1b`, `test_sse_kms_transfer_1kb`, `test_sse_kms_transfer_1MB`, `test_sse_kms_transfer_13b`, `test_sse_kms_default_upload_1b`, `test_sse_kms_default_upload_1kb`, `test_sse_kms_default_upload_1mb`, `test_sse_kms_default_upload_8mb`, `test_sse_kms_multipart_upload`, `test_sse_kms_multipart_invalid_chunks_1`, `test_sse_kms_multipart_invalid_chunks_2`, `test_sse_kms_post_object_authenticated_request`, `test_sse_kms_default_post_object_authenticated_request`, `test_copy_enc`, `test_copy_part_enc`, `test_put_bucket_encryption_kms`, `test_get_bucket_encryption_kms`, `test_delete_bucket_encryption_kms`, `test_put_obj_enc_conflict_c_kms`, `test_put_obj_enc_conflict_c_s3`, `test_put_obj_enc_conflict_s3_kms`, `test_put_obj_enc_conflict_bad_enc_kms`, `test_bucket_policy_put_obj_kms_noenc`, `test_bucket_policy_put_obj_s3_kms`, `test_bucket_policy_put_obj_kms_s3`, `test_bucket_policy_put_obj_s3_noenc`

---

## Batch 18: Checksums & Object Attributes

**Priority:** Medium-Low. Additional checksum algorithms (CRC32, CRC32C, SHA-1, SHA-256, CRC64NVME) and the `GetObjectAttributes` API are required for integrity verification and SDK-level object inspection without a full GET.

### Schema Change

```sql
ALTER TABLE haystack ADD COLUMN checksum_algorithm TEXT;  -- 'CRC32', 'CRC32C', 'SHA1', 'SHA256', 'CRC64NVME'
ALTER TABLE haystack ADD COLUMN checksum_value TEXT;
```

### Changes Required

**Additional checksums:**
- On PUT: if `x-amz-checksum-algorithm` header is present, compute the specified checksum over the request body and compare against `x-amz-checksum-{algorithm}` header value; `400 BadDigest` on mismatch
- Store checksum algorithm and value in metadata
- Return `x-amz-checksum-{algorithm}` header on GET/HEAD
- Supported algorithms: `CRC32`, `CRC32C`, `SHA1`, `SHA256`, `CRC64NVME`
- Multipart: each UploadPart may supply a part-level checksum; CompleteMultipartUpload computes the composite checksum (checksum of concatenated part checksums, per AWS spec); `x-amz-checksum-type: COMPOSITE` or `FULL_OBJECT`
- Helper format: `x-amz-sdk-checksum-algorithm` and related headers used by AWS SDKs; parse and treat as equivalent to `x-amz-checksum-algorithm`
- POST object checksum: `x-amz-checksum-*` form field

**GetObjectAttributes** (`GET /s3/{bucket}/{key}?attributes`):
- `x-amz-object-attributes` request header specifies which attributes to return: `ETag`, `Checksum`, `ObjectParts`, `StorageClass`, `ObjectSize`
- Response: `<GetObjectAttributesResponse>` XML with requested fields
- For multipart objects: `ObjectParts` includes `TotalPartsCount` and each `Part` with `PartNumber`, `Size`, `ChecksumValue`
- Pagination: `x-amz-max-parts` and `x-amz-part-number-marker` for large part lists
- Works on versioned objects with `?versionId=...`
- Works on SSE-C objects with customer key headers
- Works with version ID to return versioned object attributes

**ETag + checksum cross-check:**
- `test_multipart_reupload_checksum_and_etag`: re-uploading a multipart part must update the stored checksum and ETag; previous ETag must not be served after re-upload

### Ceph Tests Targeted

`test_object_checksum_sha256`, `test_object_checksum_crc64nvme`, `test_multipart_checksum_sha256`, `test_multipart_use_cksum_helper_crc32`, `test_multipart_use_cksum_helper_crc32c`, `test_multipart_use_cksum_helper_crc64nvme`, `test_multipart_use_cksum_helper_sha1`, `test_multipart_use_cksum_helper_sha256`, `test_get_object_attributes`, `test_get_checksum_object_attributes`, `test_get_multipart_checksum_object_attributes`, `test_get_multipart_object_attributes`, `test_get_paginated_multipart_object_attributes`, `test_get_single_multipart_object_attributes`, `test_get_versioned_object_attributes`, `test_get_sse_c_encrypted_object_attributes`, `test_multipart_reupload_checksum_and_etag`

---

## Batch 19: Atomicity & Concurrency

**Priority:** Medium. Concurrent writes to the same key must be serializable — readers must see either the full old object or the full new object, never a torn write. These tests verify that warpdrive's haystack extent model and SQLite metadata writes are atomic from the client's perspective.

### Design

SQLite's WAL mode and `BEGIN IMMEDIATE` transactions provide serialization for metadata. The haystack append-only design means bytes are written before the metadata row is updated — atomicity is achieved by writing extents to the store first, then committing the metadata row in a single transaction.

### Changes Required

- `PUT` handler: write all bytes to haystack, then commit the metadata row (etag, size, key, last_modified) in a single `BEGIN IMMEDIATE` transaction; if the transaction fails, the orphaned bytes are cleaned up on next compaction
- Concurrent PUT to the same key: second writer blocks on the SQLite write lock; when it succeeds, its object is fully visible; the first writer's object is fully replaced — no partial state visible to a reader at any point
- Dual concurrent writes (`test_atomic_dual_write_*`): two writers to the same key; the reader that runs after both complete sees exactly one complete version
- Conditional write: `If-None-Match: *` under concurrency — at most one writer wins; the other gets `412`; validated for 1MB, dual-conditional, and race conditions
- GET during concurrent PUT: reader must not see a partial object — either the old ETag+size or the new one, never a mix
- Bucket-gone atomicity: a write that lands after the bucket is deleted returns `404 NoSuchBucket` immediately; no partial data persisted

### Ceph Tests Targeted

`test_atomic_write_1mb`, `test_atomic_write_4mb`, `test_atomic_write_8mb`, `test_atomic_read_1mb`, `test_atomic_read_4mb`, `test_atomic_read_8mb`, `test_atomic_dual_write_1mb`, `test_atomic_dual_write_4mb`, `test_atomic_dual_write_8mb`, `test_atomic_write_bucket_gone`, `test_atomic_conditional_write_1mb`, `test_atomic_dual_conditional_write_1mb`

---

## Batch 20: Lifecycle Rules

**Priority:** Low. Lifecycle rules automate object expiration, multipart cleanup, version transitions, and delete-marker removal. Requires a background scheduler that runs lifecycle evaluation at configurable intervals.

### Schema Change

```sql
CREATE TABLE bucket_lifecycle (
    bucket TEXT PRIMARY KEY,
    lifecycle_json TEXT NOT NULL  -- serialized XML → JSON for easy querying
);
```

### Design

A background task (tokio task or separate thread) runs every configurable interval (default 1 hour, configurable via `WARPDRIVE_LIFECYCLE_INTERVAL_SECS`). It iterates all buckets with a lifecycle config, evaluates rules against current object metadata, and performs deletions/transitions in SQLite transactions.

### Changes Required

**Lifecycle configuration:**
- `PUT /s3/{bucket}?lifecycle` — store lifecycle XML; validate rules (each rule needs ID, Status, Filter, at least one action)
- `GET /s3/{bucket}?lifecycle` — return stored config; `404 NoSuchLifecycleConfiguration` if not set
- `DELETE /s3/{bucket}?lifecycle` — remove config
- Rule ID uniqueness enforced; `400 InvalidArgument` on duplicate IDs or IDs > 255 chars
- Rule statuses: `Enabled` / `Disabled`
- Invalid date format in `Expiration`: `400 InvalidArgument`

**Expiration actions:**
- `Expiration.Days`: delete objects older than N days
- `Expiration.Date`: delete objects created before a specific date
- `Expiration.ExpiredObjectDeleteMarker`: delete orphaned delete markers (all versions deleted)
- `NoncurrentVersionExpiration.NoncurrentDays`: delete noncurrent versions older than N days
- `NoncurrentVersionExpiration.NewerNoncurrentVersions`: keep only the N most recent noncurrent versions, expire the rest
- Filter by `Prefix`, `Tag` (key+value pair), or `ObjectSize` (gt/lt filters)
- `Expiration.Days=0` is valid (expire objects immediately on the next evaluation cycle)

**Transition actions:**
- `Transition` rule moves objects to a different storage class after N days (for Warpdrive's initial impl: treat as a no-op or log; actual tiering is out of scope unless cloud storage class is configured)
- `NoncurrentVersionTransition`: same for noncurrent versions
- Cloud transition (`test_lifecycle_cloud_*`): requires `[s3 cloud]` config section; skip if not configured

**Expiration headers:**
- `x-amz-expiration` header on GET/HEAD responses when the object matches an active lifecycle expiration rule: `expiry-date="...", rule-id="..."` format

**Multipart expiration:**
- `AbortIncompleteMultipartUpload.DaysAfterInitiation`: abort in-progress uploads older than N days

### Ceph Tests Targeted

`test_lifecycle_set`, `test_lifecycle_get`, `test_lifecycle_get_no_id`, `test_lifecycle_id_too_long`, `test_lifecycle_same_id`, `test_lifecycle_invalid_status`, `test_lifecycle_set_date`, `test_lifecycle_set_invalid_date`, `test_lifecycle_set_filter`, `test_lifecycle_set_empty_filter`, `test_lifecycle_set_deletemarker`, `test_lifecycle_set_noncurrent`, `test_lifecycle_set_noncurrent_transition`, `test_lifecycle_set_multipart`, `test_lifecycle_delete`, `test_lifecycle_expiration`, `test_lifecycle_expiration_date`, `test_lifecycle_expiration_days0`, `test_lifecycle_expiration_header_head`, `test_lifecycle_expiration_header_put`, `test_lifecycle_expiration_header_and_tags_head`, `test_lifecycle_expiration_header_tags_head`, `test_lifecycle_expiration_tags1`, `test_lifecycle_expiration_tags2`, `test_lifecycle_expiration_versioned_tags2`, `test_lifecycle_expiration_versioning_enabled`, `test_lifecycle_expiration_newer_noncurrent`, `test_lifecycle_expiration_noncur_tags1`, `test_lifecycle_expiration_size_gt`, `test_lifecycle_expiration_size_lt`, `test_lifecycle_noncur_expiration`, `test_lifecycle_deletemarker_expiration`, `test_lifecycle_deletemarker_expiration_with_days_tag`, `test_lifecycle_multipart_expiration`, `test_lifecyclev2_expiration`, `test_lifecycle_transition`, `test_lifecycle_transition_set_invalid_date`, `test_lifecycle_transition_single_rule_multi_trans`, `test_lifecycle_transition_encrypted`, `test_lifecycle_noncur_transition`, `test_lifecycle_plain_null_version_current_transition`, `test_lifecycle_cloud_transition`, `test_lifecycle_cloud_transition_large_obj`, `test_lifecycle_cloud_multiple_transition`, `test_lifecycle_cloud_transition_target_by_bucket`, `test_lifecycle_cloud_transition_target_by_bucket_multiple_buckets`, `test_lifecycle_noncur_cloud_transition`, `test_delete_marker_expiration`, `test_delete_marker_suspended`

---

## Batch 21: Bucket Logging

**Priority:** Low. Bucket access logging records every request to a target bucket as log objects with a configurable prefix. The test suite has 113 tests covering configuration, flush modes (JSON/simple), concurrent updates, cleanup, and event types.

### Schema Change

```sql
CREATE TABLE bucket_logging (
    bucket TEXT PRIMARY KEY,
    target_bucket TEXT NOT NULL,
    target_prefix TEXT NOT NULL DEFAULT '',
    logging_config_json TEXT NOT NULL  -- full XML → JSON for querying fields
);
```

### Design

Log records are appended to an in-memory buffer per bucket. A background flush task writes them to the target bucket as objects with the configured prefix at regular intervals (or immediately on `?logging=flush` Ceph extension). Two log formats: simple (space-separated fields, S3 standard) and JSON (`j` suffix tests). Log object keys are timestamped and unique.

### Changes Required

**Configuration:**
- `PUT /s3/{bucket}?logging` — store logging target bucket and prefix; validate target bucket exists; `400` if target bucket not found
- `GET /s3/{bucket}?logging` — return logging config XML (or empty `<BucketLoggingStatus/>` if disabled)
- `DELETE /s3/{bucket}?logging` (via PUT with empty XML) — disable logging
- Log target must have `FULL_CONTROL` grant to the log delivery group (`http://acs.amazonaws.com/groups/s3/LogDelivery`) on its ACL — `test_bucket_logging_bucket_acl_required` and `test_bucket_logging_object_acl_required` verify this
- `test_bucket_logging_bucket_auth_type`: logging requires authenticated PUT (anonymous PUT not allowed for logging config)

**Log delivery:**
- Each S3 API request to a logged bucket produces one log record containing: bucket name, time, remote IP, requester, key, operation, HTTP status, bytes sent, object size, request ID, host ID
- Log records are buffered and flushed to the target bucket; log objects have keys like `{prefix}{timestamp}-{uniqueid}`
- Flush extensions (Ceph-specific): `POST /s3/{bucket}?logging=flush` triggers immediate flush; response returns number of records flushed
- Single-log-object mode: `?logging=flush&single=true` — all buffered records in one object instead of one per request (used by `_single` suffix tests)
- JSON format (`j` suffix tests): log in JSON Lines format; one JSON object per line
- Simple format (`s` suffix tests): space-separated fields, S3 standard log format

**Log content operations:**
- `test_bucket_logging_put_objects`, `test_bucket_logging_get_objects`, `test_bucket_logging_head_objects`, `test_bucket_logging_delete_objects`: verify that PUT, GET, HEAD, DELETE operations each produce a correct log record in the target bucket
- `test_bucket_logging_copy_objects`, `test_bucket_logging_mpu_*`: CopyObject and multipart operations produce log records
- `test_bucket_logging_multi_delete`: multi-object delete produces per-key log records
- Versioned variants: all above with versioning enabled; VersionId appears in log record

**Configuration management:**
- `test_bucket_logging_mtime`: logging config PUT updates the bucket's last-modified time
- `test_bucket_logging_notupdating_*`: disabling logging (PUT with empty config) does not produce further log records
- `test_bucket_logging_owner`: log records include the correct bucket owner
- `test_bucket_logging_multiple_prefixes`: multiple logging configs can target different prefixes in the same target bucket
- `test_bucket_logging_single_prefix`, `test_bucket_logging_partitioned_key`, `test_bucket_logging_simple_key`: prefix format variants
- `test_bucket_logging_roll_time`: log object roll-over time is configurable
- Key filter: `test_bucket_logging_key_filter_*` — log only requests whose key matches a prefix filter

**Concurrent operations:**
- `test_bucket_logging_concurrent_flush_*`: concurrent flush requests do not produce duplicate records
- `test_bucket_logging_put_concurrency`: concurrent PUTs each produce exactly one log record
- `test_bucket_logging_conf_concurrent_updating_*`: concurrent config updates do not lose records or produce partial configs

**Cleanup (cascade delete):**
- `test_bucket_logging_cleanup_bucket_deletion_*`: deleting the source bucket disables logging and cleans up config
- `test_bucket_logging_cleanup_bucket_concurrent_deletion_*`: concurrent bucket deletion safe
- `test_bucket_logging_target_cleanup_*`: deleting the target bucket causes logging to become inactive gracefully
- `test_bucket_logging_part_cleanup_*`: partial log records (from aborted requests) are not written

**Extensions and permissions:**
- `test_bucket_logging_extensions`: Ceph-specific extensions to the logging config XML
- `test_bucket_logging_permissions`: checks that the log delivery ACL grant is required
- `test_put_bucket_logging`, `test_put_bucket_logging_errors`, `test_put_bucket_logging_permissions`, `test_put_bucket_logging_policy_wildcard`, `test_put_bucket_logging_policy_wildcard_objects`, `test_put_bucket_logging_extensions`: config PUT variants
- `test_put_bucket_logging_account_j`, `test_put_bucket_logging_account_s`, `test_put_bucket_logging_tenant_j`, `test_put_bucket_logging_tenant_s`: cross-account and cross-tenant logging
- `test_rm_bucket_logging`: explicit remove logging config
- `test_bucket_logging_requester_assumed_role`: log record requester field when request is made via IAM assumed role — **depends on Vitality Console / STS integration**

**Event types:**
- `test_bucket_logging_event_type_j/s`: log records include an event type field (e.g., `REST.PUT.OBJECT`, `REST.GET.OBJECT`)

### Ceph Tests Targeted

All 113 `test_bucket_logging_*`, `test_put_bucket_logging*`, and `test_rm_bucket_logging` tests.

---

## Batch 22: Object Lock / WORM

**Priority:** Low. Object Lock prevents objects from being deleted or overwritten for a configurable retention period. Requires versioning (Batch 11) as a prerequisite. Used for compliance (WORM — Write Once Read Many) and regulatory data retention.

### Design

Object lock is configured at the bucket level (enabled at CreateBucket time) and optionally at the object level. Two protection modes: `COMPLIANCE` (cannot be shortened even by admin) and `GOVERNANCE` (admin can override with `x-amz-bypass-governance-retention: true`). Legal hold is a separate flag that prevents deletion regardless of retention period.

### Schema Change

```sql
ALTER TABLE buckets ADD COLUMN object_lock_enabled BOOLEAN NOT NULL DEFAULT false;

CREATE TABLE object_lock_config (
    bucket TEXT PRIMARY KEY,
    mode TEXT,               -- 'COMPLIANCE' or 'GOVERNANCE'
    days INTEGER,
    years INTEGER
);

CREATE TABLE object_versions_lock (
    bucket TEXT NOT NULL,
    key TEXT NOT NULL,
    version_id TEXT NOT NULL,
    mode TEXT,               -- 'COMPLIANCE' or 'GOVERNANCE'
    retain_until_date TEXT,  -- ISO 8601
    legal_hold TEXT NOT NULL DEFAULT 'OFF',  -- 'ON' or 'OFF'
    PRIMARY KEY (bucket, key, version_id)
);
```

### Changes Required

**Bucket-level object lock:**
- `CreateBucket` with `x-amz-bucket-object-lock-enabled: true` — create bucket with object lock; versioning is automatically enabled
- `PUT /s3/{bucket}?object-lock` — set default retention mode and period; `403` if object lock was not enabled at bucket creation
- `GET /s3/{bucket}?object-lock` — return current object lock configuration; `404 ObjectLockConfigurationNotFoundError` if not configured
- `test_object_lock_get_obj_lock_invalid_bucket` and `test_object_lock_put_obj_lock_invalid_bucket`: return correct error for non-object-lock-enabled buckets
- Enable object lock after bucket creation: `test_object_lock_put_obj_lock_enable_after_create` — only allowed if bucket has no objects yet
- Invalid configuration values: `test_object_lock_put_obj_lock_invalid_days/years/mode/status` → `400 MalformedXML` or `400 InvalidArgument`
- Cannot specify both Days and Years: `test_object_lock_put_obj_lock_with_days_and_years` → `400 MalformedXML`

**Object-level retention:**
- `PUT /s3/{bucket}/{key}?retention` (with `versionId`) — set retain-until-date and mode for a specific object version
- `GET /s3/{bucket}/{key}?retention` — return retention config; `404` if no retention set
- `test_object_lock_get_obj_retention_invalid_bucket`: correct error for non-lock bucket
- Retention date must be ISO 8601 format: `test_object_lock_get_obj_retention_iso8601`
- Retention override: `test_object_lock_put_obj_retention_override_default_retention` — object-level retention can override bucket default
- Extending retention (`test_object_lock_put_obj_retention_increase_period`): always allowed
- Shortening retention (`test_object_lock_put_obj_retention_shorten_period`): `403 AccessDenied` in COMPLIANCE mode; allowed with `x-amz-bypass-governance-retention` in GOVERNANCE mode
- Bypass governance: `test_object_lock_put_obj_retention_shorten_period_bypass` — verify bypass header works
- Retention with version ID: `test_object_lock_put_obj_retention_versionid`
- Invalid mode: `test_object_lock_put_obj_retention_invalid_mode` → `400`

**Object-level legal hold:**
- `PUT /s3/{bucket}/{key}?legal-hold` — set `Status: ON` or `OFF`
- `GET /s3/{bucket}/{key}?legal-hold` — return current legal hold status
- `test_object_lock_put_legal_hold_invalid_bucket`, `test_object_lock_get_legal_hold_invalid_bucket` → correct error
- `test_object_lock_put_legal_hold_invalid_status` → `400`

**Enforcement on DELETE:**
- `DELETE /s3/{bucket}/{key}?versionId=...` on a retained object: `403 AccessDenied` (COMPLIANCE), `403` without bypass (GOVERNANCE)
- `test_object_lock_delete_object_with_retention`: blocked
- `test_object_lock_delete_object_with_retention_and_marker`: creating a delete marker is allowed but deleting the retained version is not
- `test_object_lock_delete_object_with_legal_hold_on`: blocked regardless of retention period
- `test_object_lock_delete_object_with_legal_hold_off`: allowed when hold is OFF
- Multi-object delete with retention: `test_object_lock_multi_delete_object_with_retention` — returns errors for locked objects in DeleteResult
- Multipart object with retention: `test_object_lock_delete_multipart_object_with_retention`, `test_object_lock_delete_multipart_object_with_legal_hold_on`

**Mode changing:**
- `test_object_lock_changing_mode_from_compliance`: cannot change from COMPLIANCE to GOVERNANCE → `403`
- `test_object_lock_changing_mode_from_governance_with_bypass`: can change with bypass header
- `test_object_lock_changing_mode_from_governance_without_bypass`: blocked without bypass

**Versioning interaction:**
- `test_object_lock_suspend_versioning`: suspending versioning on a bucket with object lock enabled → `400 InvalidBucketState`
- `test_object_lock_uploading_obj`: uploading an object to a bucket with default retention sets retention automatically; response includes `x-amz-object-lock-mode` and `x-amz-object-lock-retain-until-date` headers
- `test_object_lock_get_obj_metadata`: HEAD returns object lock headers

### Ceph Tests Targeted

All 39 `test_object_lock_*` tests.

---

## Batch 23: Object Restore & Miscellaneous

**Priority:** Low. Object restore is relevant when cloud tiering (lifecycle transitions to a GLACIER-equivalent storage class) is in use. The misc tests cover minor error cases and Ceph-specific extensions that don't fit other batches.

### Design

`RestoreObject` is meaningful only when cloud storage classes are configured (lifecycle transitions via Batch 20). Without cloud tiering, `RestoreObject` on a standard-storage object returns `400 ObjectAlreadyInActiveTierError`. When cloud tiering is configured, restore initiates a copy from the remote tier back to local storage.

### Changes Required

**Object Restore** (`POST /s3/{bucket}/{key}?restore`):
- Parse XML body: `<RestoreRequest><Days>N</Days><GlacierJobParameters><Tier>Standard</Tier></GlacierJobParameters></RestoreRequest>`
- If object is not tiered: return `400 ObjectAlreadyInActiveTierError`
- If object is tiered: initiate restore job; return `202 Accepted`; on subsequent HEAD/GET, return `x-amz-restore: ongoing-request="true"` until done, then `x-amz-restore: ongoing-request="false", expiry-date="..."` for a temporary restore
- Permanent restore (non-expiring): `test_restore_object_permanent` — restore with no expiry
- Non-current version restore: `test_restore_noncur_obj` — restore a noncurrent version specifically
- ListObjects restore status: `?fetch-owner` includes restore status in listing; `test_list_objects_restore_status`, `test_list_object_versions_restore_status`

**Torrent (Ceph extension):**
- `GET /s3/{bucket}/{key}?torrent` — return a `.torrent` file for the object (Ceph-specific BitTorrent distribution feature)
- `test_get_object_torrent`: verify response is a valid torrent file

**Misc error cases:**
- `test_object_read_unreadable`: GET with a key containing invalid non-UTF-8 bytes returns `400 InvalidArgument` (covered in Batch 1 implementation; listed here for completeness)

### Ceph Tests Targeted

`test_restore_object_temporary`, `test_restore_object_permanent`, `test_restore_noncur_obj`, `test_list_objects_restore_status`, `test_list_object_versions_restore_status`, `test_get_object_torrent`

---

## Out of Scope (IAM lives in Vitality Console)

The following test files from the Ceph suite are explicitly out of scope for Warpdrive. They belong to Vitality Console or are not part of the core object storage contract:

| File | Reason |
|---|---|
| `test_iam.py` | IAM users, roles, policies — Vitality Console |
| `test_sts.py` | AssumeRole, federation tokens — Vitality Console |
| `test_sns.py` | Bucket event notifications — separate notification service |
| `test_s3select.py` | SQL-in-place query engine — out of scope |
| `test_s3control.py` | Multi-region access points, S3 Batch — out of scope |
| Replication tests | Cross-region replication — future |

**Note:** `test_bucket_logging_requester_assumed_role` (1 test within Batch 21) depends on IAM assumed-role context from Vitality Console. It will be skipped until Vitality Console integration is complete.

---

## Implementation Order Summary

| Batch | Focus | Prerequisite | Tests |
|---|---|---|---|
| **Pre** | Admin user bypass in auth.rs | — | — |
| **1** 🔄 | Core CRUD correctness + schema migration + header validation | Pre | ~70 |
| **2** | ListObjects V1 + V2 full spec | 1 | ~66 |
| **3** | Object metadata + conditional GET/PUT/COPY/DELETE | 1 | ~49 |
| **4** | Range GET + ranged variants + chunked encoding | 1 | ~12 |
| **5** | Multi-object delete + CopyObject fixes | 1 | ~21 |
| **6** | Multipart upload rewrite + object attributes | 1, 4 | ~27 |
| **7** | Presigned URLs (V4 + V2) + tenant/v2 presigned CORS | 1 | ~21 |
| **8** | Bucket location + CORS | 1 | ~7 |
| **9** | Tagging (bucket + object + limits + ACL-gated) | 1 | ~16 |
| **10** | Canned ACLs + header grants + Block Public Access | 1, 2 | ~45 |
| **11** | Versioning (full) + delete markers + copy/multipart versioned | 1, 2, 3 | ~31 |
| **12** | Bucket naming validation + ownership controls + usage stats | 1 | ~33 |
| **13** | Bucket policy (JSON policy engine) | 1, 2, 10 | ~34 |
| **14** | POST Object (HTML form upload) | 1 | ~33 |
| **15** | SSE-S3 (server-managed encryption) | 1 | ~15 |
| **16** | SSE-C (customer-provided keys) | 1 | ~22 |
| **17** | SSE-KMS (KMS-managed keys) | 1, 15 | ~30 |
| **18** | Checksums (CRC32/CRC32C/SHA/CRC64NVME) + GetObjectAttributes | 1, 6, 11 | ~17 |
| **19** | Atomicity & concurrency guarantees | 1 | ~12 |
| **20** | Lifecycle rules (expiration + transition + multipart cleanup) | 1, 2, 9, 11 | ~49 |
| **21** | Bucket logging (access logs to target bucket) | 1, 2, 10, 11 | ~113 |
| **22** | Object Lock / WORM | 1, 11 | ~39 |
| **23** | Object Restore + torrent | 11, 20 | ~6 |
