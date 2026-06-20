# RFC 2.3: Object Properties & Conditional Requests

**Status:** Planned  
**Parent RFC:** [RFC 2 Overview](RFC2-s3-compatibility-plan.md)  
**Date:** June 2026

---

## Priority

High. User metadata and conditional headers are required for real application patterns — caching, atomic updates, CMS workflows, and any client that uses ETags for consistency.

## Changes Required

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

## Ceph Tests Targeted

`test_object_set_get_metadata_none_to_good`, `test_object_set_get_metadata_none_to_empty`, `test_object_set_get_metadata_overwrite_to_empty`, `test_object_set_get_unicode_metadata`, `test_object_metadata_replaced_on_put`, `test_get_object_ifmatch_good`, `test_get_object_ifmatch_failed`, `test_get_object_ifnonematch_good`, `test_get_object_ifnonematch_failed`, `test_get_object_ifmodifiedsince_good`, `test_get_object_ifmodifiedsince_failed`, `test_get_object_ifunmodifiedsince_good`, `test_get_object_ifunmodifiedsince_failed`, `test_put_object_ifmatch_good`, `test_put_object_ifmatch_failed`, `test_put_object_ifmatch_overwrite_existed_good`, `test_put_object_ifmatch_nonexisted_failed`, `test_put_object_ifnonmatch_good`, `test_put_object_ifnonmatch_failed`, `test_put_object_ifnonmatch_nonexisted_good`, `test_put_object_ifnonmatch_overwrite_existed_failed`, `test_put_object_if_match`, `test_put_object_current_if_match`, `test_put_current_object_if_match`, `test_put_current_object_if_none_match`, `test_multipart_put_object_if_match`, `test_multipart_put_current_object_if_match`, `test_multipart_put_current_object_if_none_match`, `test_copy_object_ifmatch_good`, `test_copy_object_ifmatch_failed`, `test_copy_object_ifnonematch_good`, `test_copy_object_ifnonematch_failed`, `test_delete_object_if_match`, `test_delete_object_if_match_last_modified_time`, `test_delete_object_if_match_size`, `test_delete_object_current_if_match`, `test_delete_object_current_if_match_last_modified_time`, `test_delete_object_current_if_match_size`, `test_delete_object_version_if_match`, `test_delete_object_version_if_match_last_modified_time`, `test_delete_object_version_if_match_size`, `test_delete_objects_if_match`, `test_delete_objects_if_match_last_modified_time`, `test_delete_objects_if_match_size`, `test_delete_objects_current_if_match`, `test_delete_objects_current_if_match_last_modified_time`, `test_delete_objects_current_if_match_size`, `test_delete_objects_version_if_match`, `test_delete_objects_version_if_match_last_modified_time`, `test_delete_objects_version_if_match_size`
