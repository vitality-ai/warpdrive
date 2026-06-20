# RFC 2.5: Multi-Object Delete & CopyObject Correctness

**Status:** Planned  
**Parent RFC:** [RFC 2 Overview](RFC2-s3-compatibility-plan.md)  
**Date:** June 2026

---

## Priority

Medium-High. Multi-object delete is used by every S3 client for cleanup operations (e.g., `aws s3 rm --recursive`). CopyObject has correctness bugs that prevent cross-bucket operations from working.

## Changes Required

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
- CopyObject source conditionals (`x-amz-copy-source-if-*`) handled here (logic shared with RFC 2.3 implementation)
- Multipart copy invalid/improper range handling: return `400 InvalidRange` for bad `x-amz-copy-source-range`

## Ceph Tests Targeted

`test_multi_object_delete`, `test_multi_objectv2_delete`, `test_multi_object_delete_key_limit`, `test_multi_objectv2_delete_key_limit`, `test_expected_bucket_owner`, `test_object_copy_zero_size`, `test_object_copy_16m`, `test_object_copy_same_bucket`, `test_object_copy_diff_bucket`, `test_object_copy_verify_contenttype`, `test_object_copy_to_itself`, `test_object_copy_to_itself_with_metadata`, `test_object_copy_retaining_metadata`, `test_object_copy_replacing_metadata`, `test_object_copy_bucket_not_found`, `test_object_copy_key_not_found`, `test_object_copy_not_owned_bucket`, `test_object_copy_not_owned_object_bucket`, `test_upload_part_copy_percent_encoded_key`, `test_multipart_copy_improper_range`, `test_multipart_copy_invalid_range`
