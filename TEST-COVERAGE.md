# Ceph S3 Test Coverage Tracker

Tests are sourced from the Ceph `s3-tests` suite. In-scope files: `test_s3.py` (760 tests) and `test_headers.py` (48 tests) = **808 total**.

**Workflow:** Before every push, run the Ceph suite against a live server and record which tests moved from failing to passing. List only newly-passing tests per entry — do not repeat tests already listed in earlier entries.

---

## How to run the suite

```bash
cd s3-tests
pip install -e .
S3TEST_CONF=s3tests.conf pytest s3tests/functional/test_s3.py s3tests/functional/test_headers.py -v 2>&1 | tee results.txt
```

Passing count:
```bash
grep -c " PASSED" results.txt
```

---

## feat/batch-1-s3-core-ops — Batch 1 complete

**Commit:** pending  
**Branch:** `feat/batch-1-s3-core-ops`  
**RFC Batch:** Batch 1 (Core CRUD + Header Validation)  
**Newly passing:** 40

### Verified Passing

#### test_s3.py (21)
```
test_bucket_notexist
test_bucketv2_notexist
test_bucket_delete_notexist
test_bucket_delete_nonempty
test_object_write_to_nonexist_bucket
test_bucket_create_delete
test_object_read_not_exist
test_object_requestid_matches_header_on_error
test_object_head_zero_bytes
test_object_write_check_etag
test_object_write_read_update_read_delete
test_object_write_file
test_bucket_head
test_bucket_head_notexist
test_buckets_create_then_list
test_buckets_list_ctime
test_object_write_cache_control
test_object_write_expires
test_object_delete_key_bucket_gone
test_bucket_head_extended
test_object_read_unreadable
```

#### test_headers.py (19)
```
test_object_create_bad_md5_invalid_short
test_object_create_bad_md5_bad
test_object_create_bad_md5_empty
test_object_create_bad_md5_none
test_object_create_bad_expect_mismatch
test_object_create_bad_expect_empty
test_object_create_bad_expect_none
test_object_create_bad_contentlength_empty
test_object_create_bad_contentlength_negative
test_object_create_bad_contenttype_invalid
test_object_create_bad_contenttype_empty
test_object_create_bad_contenttype_none
test_bucket_create_contentlength_none
test_object_acl_create_contentlength_none
test_bucket_create_bad_expect_mismatch
test_bucket_create_bad_expect_empty
test_bucket_create_bad_contentlength_empty
test_bucket_create_bad_contentlength_negative
test_bucket_create_bad_contentlength_none
```

### Intentionally excluded (29 test_headers.py tests — not counted toward 808 total)

- **21 `_aws2` tests** — AWS SigV2 is deprecated and not implemented
- **7 `fails_on_rgw` / boto3 limitation tests** — boto3 rewrites the header being tested after signing; unfixable server-side and also fail on the reference RGW implementation
- **1 ACL test** (`test_bucket_put_bad_canned_acl`) — deferred to Batch 10

**Running total: 40 / 808**

---

## feat/batch-2-object-listing — Batch 2 complete

**Commit:** pending  
**Branch:** `feat/batch-2-object-listing`  
**RFC Batch:** Batch 2 (Object Listing — ListObjectsV1 + V2)  
**Newly passing:** 141

Key changes:
- Full `s3_list_objects_handler` for both ListObjectsV1 and ListObjectsV2
- Prefix filtering, delimiter+CommonPrefixes grouping, max-keys truncation, marker/continuation-token pagination
- `encoding-type=url` support with `s3_url_encode` helper
- Asymmetric prefix encoding fix: V1 `<Prefix>` is never URL-encoded (botocore does not URL-decode it); V2 `<Prefix>` is URL-encoded when encoding-type=url
- ListBuckets pagination (`max-buckets`, `continuation-token`)
- Auth: unauthenticated requests now return HTTP 403 `AccessDenied` XML (was 401 plain text)

### Verified Passing

#### test_s3.py (141)
```
test_basic_key_count
test_bucket_acl_canned_private_to_private
test_bucket_concurrent_set_canned_acl
test_bucket_create_exists
test_bucket_create_naming_bad_short_one
test_bucket_create_naming_bad_short_two
test_bucket_create_naming_bad_starts_nonalpha
test_bucket_create_naming_dns_dash_at_end
test_bucket_create_naming_dns_dot_dot
test_bucket_create_naming_dns_long
test_bucket_create_naming_dns_underscore
test_bucket_create_naming_good_contains_hyphen
test_bucket_create_naming_good_contains_period
test_bucket_create_naming_good_long_60
test_bucket_create_naming_good_long_61
test_bucket_create_naming_good_long_62
test_bucket_create_naming_good_long_63
test_bucket_create_naming_good_starts_alpha
test_bucket_create_naming_good_starts_digit
test_bucket_create_special_key_names
test_bucket_list_delimiter_alt
test_bucket_list_delimiter_basic
test_bucket_list_delimiter_dot
test_bucket_list_delimiter_empty
test_bucket_list_delimiter_none
test_bucket_list_delimiter_not_exist
test_bucket_list_delimiter_not_skip_special
test_bucket_list_delimiter_percentage
test_bucket_list_delimiter_prefix
test_bucket_list_delimiter_prefix_ends_with_delimiter
test_bucket_list_delimiter_prefix_underscore
test_bucket_list_delimiter_unreadable
test_bucket_list_delimiter_whitespace
test_bucket_list_distinct
test_bucket_list_empty
test_bucket_list_encoding_basic
test_bucket_list_long_name
test_bucket_list_many
test_bucket_list_marker_after_list
test_bucket_list_marker_empty
test_bucket_list_marker_none
test_bucket_list_marker_not_in_list
test_bucket_list_marker_unreadable
test_bucket_list_maxkeys_invalid
test_bucket_list_maxkeys_none
test_bucket_list_maxkeys_one
test_bucket_list_maxkeys_zero
test_bucket_list_objects_anonymous_fail
test_bucket_list_prefix_alt
test_bucket_list_prefix_basic
test_bucket_list_prefix_delimiter_alt
test_bucket_list_prefix_delimiter_basic
test_bucket_list_prefix_delimiter_delimiter_not_exist
test_bucket_list_prefix_delimiter_prefix_delimiter_not_exist
test_bucket_list_prefix_delimiter_prefix_not_exist
test_bucket_list_prefix_empty
test_bucket_list_prefix_none
test_bucket_list_prefix_not_exist
test_bucket_list_prefix_unreadable
test_bucket_list_unordered
test_bucket_listv2_both_continuationtoken_startafter
test_bucket_listv2_continuationtoken
test_bucket_listv2_continuationtoken_empty
test_bucket_listv2_delimiter_alt
test_bucket_listv2_delimiter_basic
test_bucket_listv2_delimiter_dot
test_bucket_listv2_delimiter_empty
test_bucket_listv2_delimiter_none
test_bucket_listv2_delimiter_not_exist
test_bucket_listv2_delimiter_percentage
test_bucket_listv2_delimiter_prefix
test_bucket_listv2_delimiter_prefix_ends_with_delimiter
test_bucket_listv2_delimiter_prefix_underscore
test_bucket_listv2_delimiter_unreadable
test_bucket_listv2_delimiter_whitespace
test_bucket_listv2_encoding_basic
test_bucket_listv2_fetchowner_defaultempty
test_bucket_listv2_fetchowner_empty
test_bucket_listv2_fetchowner_notempty
test_bucket_listv2_many
test_bucket_listv2_maxkeys_none
test_bucket_listv2_maxkeys_one
test_bucket_listv2_maxkeys_zero
test_bucket_listv2_objects_anonymous_fail
test_bucket_listv2_prefix_alt
test_bucket_listv2_prefix_basic
test_bucket_listv2_prefix_delimiter_alt
test_bucket_listv2_prefix_delimiter_basic
test_bucket_listv2_prefix_delimiter_delimiter_not_exist
test_bucket_listv2_prefix_delimiter_prefix_delimiter_not_exist
test_bucket_listv2_prefix_delimiter_prefix_not_exist
test_bucket_listv2_prefix_empty
test_bucket_listv2_prefix_none
test_bucket_listv2_prefix_not_exist
test_bucket_listv2_prefix_unreadable
test_bucket_listv2_startafter_after_list
test_bucket_listv2_startafter_not_in_list
test_bucket_listv2_startafter_unreadable
test_bucket_listv2_unordered
test_bucket_recreate_not_overriding
test_expected_bucket_owner
test_get_object_ifmatch_good
test_get_object_ifmodifiedsince_good
test_get_object_ifnonematch_failed
test_get_object_ifunmodifiedsince_failed
test_list_buckets_paginated
test_multi_object_delete
test_multi_objectv2_delete
test_object_acl_full_control_verify_owner
test_object_anon_put
test_object_metadata_replaced_on_put
test_object_put_authenticated
test_object_raw_authenticated
test_object_raw_authenticated_bucket_acl
test_object_raw_authenticated_bucket_gone
test_object_raw_authenticated_object_acl
test_object_raw_authenticated_object_gone
test_object_raw_get_object_acl
test_object_raw_get_x_amz_expires_out_max_range
test_object_raw_get_x_amz_expires_out_positive_range
test_object_raw_get_x_amz_expires_out_range_zero
test_object_raw_put_authenticated_expired
test_object_set_get_metadata_none_to_empty
test_object_set_get_metadata_none_to_good
test_object_set_get_metadata_overwrite_to_empty
test_object_write_with_chunked_transfer_encoding
test_post_object_condition_is_case_sensitive
test_post_object_empty_conditions
test_post_object_expires_is_case_sensitive
test_post_object_invalid_content_length_argument
test_post_object_invalid_date_format
test_post_object_missing_conditions_list
test_post_object_missing_content_length_argument
test_post_object_missing_expires_condition
test_post_object_missing_signature
test_post_object_no_key_specified
test_post_object_upload_size_below_minimum
test_post_object_upload_size_limit_exceeded
test_put_object_ifmatch_good
test_put_object_ifmatch_overwrite_existed_good
test_put_object_ifnonmatch_good
test_put_object_ifnonmatch_nonexisted_good
```

### Intentionally deferred (4 tests)

- `test_bucket_list_return_data` — requires `x-amz-acl` / ACL read support (Batch 10)
- `test_bucket_list_return_data_versioning` — requires object versioning (Batch 3)
- `test_bucket_list_objects_anonymous` — requires public-read ACL on bucket (Batch 10)
- `test_bucket_listv2_objects_anonymous` — requires public-read ACL on bucket (Batch 10)

**Running total: 181 / 808**

---

## feat/batch-3-object-properties — Batch 3 complete

**Commit:** pending  
**Branch:** `feat/batch-3-object-properties`  
**RFC Batch:** Batch 3 (Object Properties & Conditional Requests)  
**Newly passing:** 25

Key changes:
- SigV4 canonical-header sort fix: sort by header name only, not full `"name:value"` string — critical for headers like `x-amz-copy-source` vs `x-amz-copy-source-if-match`
- Non-ASCII metadata: use `from_utf8_lossy()` for header value parsing (boto3 sends UTF-8); respond with Latin-1 bytes for urllib3 1.26 round-trip
- Conditional PUT/CompleteMultipartUpload: `If-Match` / `If-None-Match`
- Conditional GET: `If-Match` (→ 412), `If-None-Match` (→ 304), `If-Modified-Since` (→ 304), `If-Unmodified-Since` (→ 412) with ETag + Last-Modified in 304 responses
- Conditional DELETE (single): `If-Match`, `x-amz-if-match-last-modified-time`, `x-amz-if-match-size`
- Conditional DELETE multi-object: per-object `<ETag>`, `<LastModifiedTime>`, `<Size>` in XML body
- CopyObject conditionals: `x-amz-copy-source-if-match` / `x-amz-copy-source-if-none-match`
- CopyObject self-copy check: src == dst without REPLACE directive → 400 InvalidRequest
- CompleteMultipartUpload: persist ETag via `put_object_full` so subsequent conditional requests see the correct ETag
- `extract_xml_tag`, `normalize_etag`, `s3_precondition_failed`, `parse_http_date` helpers added

Note: 9 tests confirmed passing in full-suite run; 16 additional tests verified passing in isolation — they ERROR in the full suite due to cascade failures from unsupported object-lock tests that run just before them in the suite order. The underlying implementations are correct.

### Verified Passing

#### Newly passing in full suite run (9)
```
test_get_object_ifmatch_failed
test_get_object_ifmodifiedsince_failed
test_get_object_ifnonematch_good
test_get_object_ifunmodifiedsince_good
test_object_set_get_unicode_metadata
test_put_object_ifmatch_failed
test_put_object_ifmatch_nonexisted_failed
test_put_object_ifnonmatch_failed
test_put_object_ifnonmatch_overwrite_existed_failed
```

#### Verified passing in isolation only (16 — cascade-shadowed in full suite)
```
test_copy_object_ifmatch_good
test_copy_object_ifmatch_failed
test_copy_object_ifnonematch_good
test_copy_object_ifnonematch_failed
test_delete_object_if_match
test_delete_object_if_match_last_modified_time
test_delete_object_if_match_size
test_delete_objects_if_match
test_delete_objects_if_match_last_modified_time
test_delete_objects_if_match_size
test_multipart_put_object_if_match
test_object_copy_replacing_metadata
test_object_copy_retaining_metadata
test_object_copy_to_itself
test_object_copy_to_itself_with_metadata
test_put_object_if_match
```

### Intentionally deferred (4 tests)

- `test_put_current_object_if_none_match` — requires versioning (Batch 7)
- `test_multipart_put_current_object_if_none_match` — requires versioning (Batch 7)
- `test_put_current_object_if_match` — requires versioning (Batch 7)
- `test_multipart_put_current_object_if_match` — requires versioning (Batch 7)

**Running total: 206 / 808**

---

## feat/batch-4-range-requests — Batch 4 complete

**Branch:** `feat/batch-4-range-requests`  
**RFC Batch:** Batch 4 (Range Requests & Content-Encoding)  
**Newly passing:** 4

Key changes:
- Range GET: tri-state `RangeResult` enum — distinguishes no-header (200) from unsatisfiable (416) from valid (206)
- Suffix range `bytes=-N` now returns last N bytes correctly
- Out-of-bounds ranges (start >= object size, or any range on empty object) return 416 `InvalidRange`
- `Content-Encoding` stored on PUT, returned on GET/HEAD, copied on CopyObject
- `aws-chunked` stripped from stored `Content-Encoding` (transport-only encoding per S3 spec)

### Verified Passing

```
test_ranged_request_invalid_range
test_ranged_request_empty_object
test_ranged_request_return_trailing_bytes_response_code
test_object_content_encoding_aws_chunked
```

### Already passing (no change needed)

```
test_ranged_request_response_code
test_ranged_request_skip_leading_bytes_response_code
test_ranged_big_request_response_code
```

### Intentionally deferred (2 tests)

- `test_100_continue` — second assertion requires public-write ACL (Batch 10)
- `test_read_through` — skipped by test suite (requires `[s3 cloud]` config section)

**Running total: 210 / 808**

---

## feat/batch-5-multi-delete-copy — Batch 5 complete

**Branch:** `feat/batch-5-multi-delete-copy`  
**RFC Batch:** Batch 5 (Multi-Object Delete & CopyObject fixes)  
**Newly passing:** 5

Key changes:
- `DeleteObjects`: enforce 1000-key limit → 400 `MalformedXML`
- `ListObjectVersions`: respect `MaxKeys` and return paginated results with `IsTruncated`/`NextKeyMarker` (was returning all objects in one batch, breaking teardown cleanup for large buckets)
- `xml_unescape` helper: XML-unescape keys parsed from `delete_objects` request body — fixes teardown failures for buckets containing keys with `&`, `<`, `>` characters
- `percent_decode` helper: URL-decode `x-amz-copy-source` key before lookup (so `anyfilename%25.txt` in header → `anyfilename%.txt` lookup)
- `s3_upload_part_copy_handler`: new handler for UploadPartCopy (`PUT ?partNumber&uploadId + x-amz-copy-source`); validates `x-amz-copy-source-range` — malformed format → 400 `InvalidArgument`, out-of-bounds → 416 `InvalidRange`
- Dispatch: UploadPartCopy routed before regular UploadPart and CopyObject

### Newly passing (5)

```
test_multi_object_delete_key_limit
test_multi_objectv2_delete_key_limit
test_upload_part_copy_percent_encoded_key
test_multipart_copy_improper_range
test_multipart_copy_invalid_range
```

### Already passing before this batch (13)

```
test_multi_object_delete
test_multi_objectv2_delete
test_object_copy_zero_size
test_object_copy_16m
test_object_copy_same_bucket
test_object_copy_diff_bucket
test_object_copy_verify_contenttype
test_object_copy_to_itself
test_object_copy_to_itself_with_metadata
test_object_copy_retaining_metadata
test_object_copy_replacing_metadata
test_object_copy_bucket_not_found
test_object_copy_key_not_found
```

### Intentionally deferred (2 tests)

- `test_object_copy_not_owned_bucket` — requires real multi-user auth; both `s3 main` and `s3 alt` share the same `adminkey` credentials
- `test_object_copy_not_owned_object_bucket` — same reason

**Running total: 215 / 808**

---

## feat/batch-6-multipart-upload — Batch 6 complete

**Branch:** `feat/batch-6-multipart-upload`  
**RFC Batch:** Batch 6 (Multipart Upload Full Correctness)  
**Newly passing:** 17

Key changes:
- New SQLite tables `multipart_uploads` (tracks in-flight uploads with status/final_etag) and `multipart_parts` (per-part ETag/size/extents_blob) — replaces fake-key approach
- `parts_manifest TEXT` column on `objects` table (JSON: `[{"n":1,"sz":5242880,"ext":[[0,5242880]]},...]`)
- `CreateMultipartUpload`: stores ContentType + x-amz-meta-* headers in `multipart_uploads`
- `UploadPart`: INSERT OR REPLACE into `multipart_parts` so re-upload of same part number overwrites previous (no UNIQUE constraint violations)
- `CompleteMultipartUpload`: validates part ETags, enforces 5 MB minimum on non-last parts, computes real S3 multipart ETag (`hex(md5(bytes1||bytes2||...))-N`), idempotent (second call returns stored ETag), deduplicates PartNumbers keeping last occurrence (handles concurrent re-uploads)
- `AbortMultipartUpload`: validates uploadId is in_progress, queues part extents for GC
- `ListMultipartUploads`: dispatched from list-objects handler when `?uploads` query param present
- `GetObjectAttributes`: returns ETag (no quotes), ObjectSize, StorageClass, ObjectParts with `<PartsCount>` (botocore locationName), paginated via `?max-parts&part-number-marker`
- `GET/HEAD ?partNumber`: streams specific part bytes for multipart objects; 400 InvalidPart for out-of-range on multipart; partNumber=1 OK for single-part objects, >1 → 400 InvalidPart

### Verified Passing

```
test_multipart_upload_empty
test_multipart_upload_complete_without_create
test_multipart_upload_small
test_multipart_upload
test_multipart_upload_resend_part
test_multipart_get_part
test_multipart_single_get_part
test_multipart_resend_first_finishes_last
test_multipart_upload_size_too_small
test_multipart_upload_missing_part
test_multipart_upload_incorrect_etag
test_abort_multipart_upload_not_found
test_list_multipart_upload
test_non_multipart_get_part
test_get_multipart_object_attributes
test_get_paginated_multipart_object_attributes
test_get_single_multipart_object_attributes
```

### Intentionally deferred (1 test)

- `test_list_multipart_upload_owner` — both `s3 main` and `s3 alt` share the same `adminkey`/`admin` user_id; test requires distinct DisplayNames per initiator

**Running total: 232 / 808**

---

<!-- Template for next entry — copy and fill in before each push:

## <branch-name> — <short description>

**Commit:** `<hash>`  
**Branch:** `<branch>`  
**RFC Batch:** Batch N (<name>)  
**Newly passing:** N

### Verified Passing

```
test_foo
test_bar
...
```

**Running total: X / 808**

-->
