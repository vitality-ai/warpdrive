# RFC 2.17: Server-Side Encryption — SSE-KMS

**Status:** Planned  
**Parent RFC:** [RFC 2 Overview](RFC2-s3-compatibility-plan.md)  
**Date:** June 2026

---

## Priority

Low. SSE-KMS uses an AWS KMS key (or compatible KMS) to envelope-encrypt the data key. For Warpdrive, this requires either integrating with a real KMS endpoint or running a local KMS mock (e.g., localstack). The tests require `[s3 kms]` config section with a valid key ID.

## Design

Warpdrive defers to an external KMS endpoint configured via `WARPDRIVE_KMS_ENDPOINT` and `WARPDRIVE_KMS_KEY_ID`. If the env vars are absent, SSE-KMS requests return `400 InvalidArgument`.

## Changes Required

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

## Ceph Tests Targeted

`test_sse_kms_present`, `test_sse_kms_no_key`, `test_sse_kms_not_declared`, `test_sse_kms_method_head`, `test_sse_kms_read_declare`, `test_sse_kms_transfer_1b`, `test_sse_kms_transfer_1kb`, `test_sse_kms_transfer_1MB`, `test_sse_kms_transfer_13b`, `test_sse_kms_default_upload_1b`, `test_sse_kms_default_upload_1kb`, `test_sse_kms_default_upload_1mb`, `test_sse_kms_default_upload_8mb`, `test_sse_kms_multipart_upload`, `test_sse_kms_multipart_invalid_chunks_1`, `test_sse_kms_multipart_invalid_chunks_2`, `test_sse_kms_post_object_authenticated_request`, `test_sse_kms_default_post_object_authenticated_request`, `test_copy_enc`, `test_copy_part_enc`, `test_put_bucket_encryption_kms`, `test_get_bucket_encryption_kms`, `test_delete_bucket_encryption_kms`, `test_put_obj_enc_conflict_c_kms`, `test_put_obj_enc_conflict_c_s3`, `test_put_obj_enc_conflict_s3_kms`, `test_put_obj_enc_conflict_bad_enc_kms`, `test_bucket_policy_put_obj_kms_noenc`, `test_bucket_policy_put_obj_s3_kms`, `test_bucket_policy_put_obj_kms_s3`, `test_bucket_policy_put_obj_s3_noenc`
