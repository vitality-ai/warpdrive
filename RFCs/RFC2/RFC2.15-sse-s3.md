# RFC 2.15: Server-Side Encryption — SSE-S3

**Status:** Planned  
**Parent RFC:** [RFC 2 Overview](RFC2-s3-compatibility-plan.md)  
**Date:** June 2026

---

## Priority

Medium-Low. SSE-S3 uses server-managed AES-256 keys transparent to the client. The client requests encryption with `x-amz-server-side-encryption: AES256`; the server encrypts before writing and decrypts on read. For Warpdrive's initial implementation, the "encryption" can be a passthrough with correct header echoing, with real AES added later when a key management approach is chosen.

## Design

Phase 1 (this RFC): enforce correct request/response header handling and reject conflicting encryption headers. Actual AES-256 encryption of the stored bytes is optional in Phase 1 — the tests primarily check that the API surface is correct.

## Changes Required

- Accept `x-amz-server-side-encryption: AES256` on PUT; echo `x-amz-server-side-encryption: AES256` on GET/HEAD
- Store `sse_type` in metadata columns (`sse-s3`, `sse-kms`, `sse-c`, or null)
- Default bucket encryption: `PUT /s3/{bucket}?encryption` — store default SSE-S3 or SSE-KMS configuration; apply to all new objects that don't specify encryption explicitly
- `GET /s3/{bucket}?encryption` — return encryption configuration
- `DELETE /s3/{bucket}?encryption` — remove default encryption
- Reject `x-amz-server-side-encryption: aws:kms` with an invalid algorithm header → `400 InvalidArgument`
- Reject conflicting headers (e.g., SSE-C `x-amz-server-side-encryption-customer-*` with SSE-S3 header simultaneously)
- Multipart upload with SSE-S3: all parts must use the same SSE type; CompleteMultipartUpload returns correct SSE header

## Ceph Tests Targeted

`test_sse_s3_default_upload_1b`, `test_sse_s3_default_upload_1kb`, `test_sse_s3_default_upload_1mb`, `test_sse_s3_default_upload_8mb`, `test_sse_s3_encrypted_upload_1b`, `test_sse_s3_encrypted_upload_1kb`, `test_sse_s3_encrypted_upload_1mb`, `test_sse_s3_encrypted_upload_8mb`, `test_sse_s3_default_method_head`, `test_sse_s3_default_multipart_upload`, `test_sse_s3_default_post_object_authenticated_request`, `test_bucket_policy_put_obj_s3_incorrect_algo_sse_s3`, `test_put_bucket_encryption_s3`, `test_get_bucket_encryption_s3`, `test_delete_bucket_encryption_s3`
