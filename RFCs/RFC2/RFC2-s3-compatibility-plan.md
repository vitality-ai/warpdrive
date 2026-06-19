# RFC 2: S3 Compatibility Plan for Warpdrive

**Status:** Draft  
**Date:** June 2026  
**Authors:** Tejas-ChandraShekarRaju

---

## Abstract

This RFC defines the phased plan for achieving S3 API compatibility in Warpdrive. Tests are sourced from the Ceph S3 test suite (`s3-tests`) and organized into batches by priority — starting with the operations used by virtually every S3 client and progressing toward advanced features. Each batch identifies what currently exists, what is broken or missing, and which Ceph tests it unlocks.

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

`test_bucket_create_delete`, `test_object_write_read_update_read_delete`, `test_object_head_zero_bytes`, `test_object_write_check_etag`, `test_bucket_head`, `test_bucket_head_notexist`, `test_bucket_notexist`, `test_bucketv2_notexist`, `test_bucket_delete_notexist`, `test_bucket_delete_nonempty`, `test_object_read_not_exist`, `test_object_write_to_nonexist_bucket`, `test_buckets_create_then_list`, `test_buckets_list_ctime`, `test_object_write_cache_control`, `test_object_write_expires`, `test_object_write_file`, `test_object_requestid_matches_header_on_error`, `test_bucket_head_extended`

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

### Ceph Tests Targeted

All ~65 `test_bucket_list_*` and `test_bucket_listv2_*` tests, including: empty, distinct, many, delimiter variants, prefix variants, maxkeys variants, marker/continuation-token variants, encoding, unordered, fetchowner, `test_basic_key_count`, `test_bucket_list_return_data`, `test_bucket_list_return_data_versioning`, `test_bucket_list_objects_anonymous*`

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

### Ceph Tests Targeted

`test_object_set_get_metadata_none_to_good`, `test_object_set_get_metadata_none_to_empty`, `test_object_set_get_metadata_overwrite_to_empty`, `test_object_set_get_unicode_metadata`, `test_object_metadata_replaced_on_put`, `test_get_object_ifmatch_good`, `test_get_object_ifmatch_failed`, `test_get_object_ifnonematch_good`, `test_get_object_ifnonematch_failed`, `test_get_object_ifmodifiedsince_good`, `test_get_object_ifmodifiedsince_failed`, `test_get_object_ifunmodifiedsince_good`, `test_get_object_ifunmodifiedsince_failed`, `test_put_object_ifmatch_good`, `test_put_object_ifmatch_failed`, `test_put_object_ifmatch_overwrite_existed_good`, `test_put_object_ifmatch_nonexisted_failed`, `test_put_object_ifnonmatch_good`, `test_put_object_ifnonmatch_failed`, `test_put_object_ifnonmatch_nonexisted_good`, `test_put_object_ifnonmatch_overwrite_existed_failed`

---

## Batch 4: Range Requests & Transfer Encoding

**Priority:** Medium-High. Range GET is critical for large-file resumable downloads, video streaming, and is required for multipart object GET-by-part (Batch 6). `100-continue` and `aws-chunked` are what the AWS CLI and SDK emit by default.

### Changes Required

**Range GET:**
- Parse `Range: bytes=start-end` header
- Map the byte range across the extent list stored in SQLite (warpdrive's disaggregated offset-size model makes this natural — slice the extents to cover `[start, end]`)
- Return `206 Partial Content` with `Content-Range: bytes start-end/total` and `Content-Length: (end-start+1)`
- Return `416 Range Not Satisfiable` for invalid ranges

**100-Continue:**
- Handle `Expect: 100-continue` header correctly — actix-web handles most of this, but the handler must not reject the header or stall

**aws-chunked transfer encoding:**
- Detect `Content-Encoding: aws-chunked` or `x-amz-content-sha256: STREAMING-AWS4-HMAC-SHA256-PAYLOAD`
- Decode the chunked body format (each chunk is prefixed with `{hex-size};chunk-signature=...\r\n`) before passing bytes to storage

### Ceph Tests Targeted

`test_100_continue`, `test_100_continue_error_retry`, `test_object_content_encoding_aws_chunked`, `test_object_write_with_chunked_transfer_encoding`, range-based subtests within `test_multipart_get_part`

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

**CopyObject fixes:**
- Fix source bucket parsing: `x-amz-copy-source` is `/source-bucket/source-key` (URL path with leading slash, percent-encoded) — current code uses `splitn(2, '/')` and gets it wrong
- Cross-bucket copy: source bucket may differ from destination bucket; use separate DB contexts for source read vs. destination write
- `x-amz-metadata-directive: COPY` (default) — copy source metadata to destination
- `x-amz-metadata-directive: REPLACE` — use new headers from the COPY request as metadata
- Real ETag on copy result (MD5 of copied data, not placeholder)

### Ceph Tests Targeted

`test_multi_object_delete`, `test_multi_objectv2_delete`, `test_multi_object_delete_key_limit`, `test_expected_bucket_owner`, `test_object_copy_zero_size`, `test_object_copy_16m`, `test_object_copy_same_bucket`, `test_object_copy_diff_bucket`, `test_object_copy_verify_contenttype`, `test_object_copy_to_itself`, `test_object_copy_to_itself_with_metadata`, `test_object_copy_retaining_metadata`, `test_object_copy_replacing_metadata`, `test_object_copy_bucket_not_found`, `test_object_copy_key_not_found`

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
- Remove the old part-as-fake-key logic entirely

### Ceph Tests Targeted

`test_multipart_upload_empty`, `test_multipart_upload_complete_without_create`, `test_multipart_upload_small`, `test_multipart_upload`, `test_multipart_upload_multiple_sizes`, `test_multipart_upload_resend_part`, `test_multipart_upload_contents`, `test_multipart_upload_overwrite_existing_object`, `test_multipart_upload_size_too_small`, `test_multipart_upload_missing_part`, `test_multipart_upload_incorrect_etag`, `test_abort_multipart_upload`, `test_abort_multipart_upload_not_found`, `test_list_multipart_upload`, `test_list_multipart_upload_owner`, `test_multipart_get_part`, `test_multipart_single_get_part`, `test_non_multipart_get_part`, `test_multipart_copy_small`, `test_multipart_copy_multiple_sizes`, `test_multipart_copy_without_range`, `test_multipart_copy_special_names`, `test_atomic_multipart_upload_write`, `test_multipart_resend_first_finishes_last`

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

### Ceph Tests Targeted

`test_object_raw_get`, `test_object_raw_get_bucket_gone`, `test_object_raw_get_object_gone`, `test_object_raw_authenticated`, `test_object_raw_authenticated_bucket_gone`, `test_object_raw_authenticated_object_gone`, `test_object_raw_get_x_amz_expires_not_expired`, `test_object_raw_get_x_amz_expires_out_range_zero`, `test_object_raw_get_x_amz_expires_out_max_range`, `test_object_raw_get_x_amz_expires_out_positive_range`, `test_object_raw_put_authenticated_expired`, `test_object_presigned_put_object_with_acl`, `test_object_raw_response_headers`

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
- `GET /s3/{bucket}?tagging` — return tags XML
- `DELETE /s3/{bucket}?tagging` — remove all tags
- Same three routes for `PUT/GET/DELETE /s3/{bucket}/{key}?tagging`

### Ceph Tests Targeted

`test_set_bucket_tagging` and all bucket/object tagging variant tests

---

## Batch 10: ACL — Canned ACLs, Single Admin User

**Priority:** Medium-Low. With a single admin user, full per-user ACL evaluation is not needed yet. Implement canned ACLs to support public-read buckets/objects and anonymous access patterns.

### Design

ACL is hardcoded for the admin user: admin always has full access to everything they own. ACL only gates unauthenticated (anonymous) access to bucket/object resources. No per-user grant table needed at this stage.

### Schema Change

```sql
ALTER TABLE haystack ADD COLUMN acl TEXT DEFAULT 'private';

CREATE TABLE bucket_acl (
    bucket TEXT PRIMARY KEY,
    acl TEXT NOT NULL DEFAULT 'private'
);
```

### Changes Required

- On PUT (object or bucket): parse `x-amz-acl` header; store canned ACL value (`private`, `public-read`, `public-read-write`, `authenticated-read`)
- On anonymous GET (no `Authorization` header): check stored ACL; allow if `public-read` or `public-read-write`; return `403 AccessDenied` if `private`
- `PUT /s3/{bucket}?acl` — update bucket ACL
- `GET /s3/{bucket}?acl` — return bucket ACL as XML `<AccessControlPolicy>` with owner and grant list
- `PUT /s3/{bucket}/{key}?acl` — update object ACL
- `GET /s3/{bucket}/{key}?acl` — return object ACL XML
- List-buckets anonymous: `403` (S3 never allows anonymous list-all-my-buckets)

### Ceph Tests Targeted

`test_object_anon_put`, `test_object_anon_put_write_access`, `test_object_put_authenticated`, `test_access_bucket_private_object_private`, `test_access_bucket_private_object_publicread`, `test_access_bucket_publicread_object_private`, `test_access_bucket_publicread_object_publicread`, `test_access_bucket_publicreadwrite_*`, `test_bucket_acl_*`, `test_object_acl_*`, `test_list_buckets_anonymous`, `test_list_buckets_invalid_auth`, `test_list_buckets_bad_auth`

---

## Batch 11: Versioning

**Priority:** Low. Significant schema change. Every PUT creates a new version row rather than overwriting. Required for strong consistency guarantees and point-in-time recovery.

### Changes Required

- New `versioning_state` column per bucket (`disabled`, `enabled`, `suspended`)
- Versioning-aware object table: all writes produce a new version with a UUID `VersionId`; reads without version ID return the latest
- Delete with versioning enabled creates a delete marker (no data deleted)
- `GET /s3/{bucket}/{key}?versionId=...` — retrieve specific version
- `DELETE /s3/{bucket}/{key}?versionId=...` — permanently delete a specific version
- `GET /s3/{bucket}?versions` — ListObjectVersions
- `PUT /s3/{bucket}?versioning` — enable/suspend versioning

### Ceph Tests Targeted

`test_versioning_bucket_create_suspend`, `test_versioning_obj_create_read_remove`, `test_versioning_obj_create_read_remove_head`, `test_versioning_stack_delete_merkers`, `test_versioning_obj_plain_null_version_*`, `test_versioning_obj_suspend_versions`, `test_versioning_obj_create_versions_remove_all`, `test_versioning_concurrent_multi_object_delete`

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
| SSE-C / SSE-KMS tests | Server-side encryption — future, post-ACL |
| Replication tests | Cross-region replication — future |

---

## Implementation Order Summary

| Batch | Focus | Prerequisite |
|---|---|---|
| **Pre** | Admin user bypass in auth.rs | — |
| **1** | Core CRUD correctness + schema migration | Pre |
| **2** | ListObjects V1 + V2 full spec | 1 |
| **3** | Object metadata + conditional requests | 1 |
| **4** | Range GET + chunked encoding | 1 |
| **5** | Multi-object delete + CopyObject fixes | 1 |
| **6** | Multipart upload rewrite | 1, 4 |
| **7** | Presigned URLs | 1 |
| **8** | Bucket location + CORS | 1 |
| **9** | Tagging | 1 |
| **10** | Canned ACLs | 1, 2 |
| **11** | Versioning | 1, 2, 3 |
