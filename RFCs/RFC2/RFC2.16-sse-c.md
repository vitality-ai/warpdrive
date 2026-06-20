# RFC 2.16: Server-Side Encryption — SSE-C (Customer-Provided Keys)

**Status:** Planned  
**Parent RFC:** [RFC 2 Overview](RFC2-s3-compatibility-plan.md)  
**Date:** June 2026

---

## Priority

Medium-Low. SSE-C lets the client supply the AES-256 key on every request. The server encrypts with that key and never stores it — the client must re-supply the same key on GET/HEAD/CopyObject or receive `403`.

## Changes Required

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

## Ceph Tests Targeted

`test_encryption_sse_c_present`, `test_encryption_sse_c_no_key`, `test_encryption_sse_c_no_md5`, `test_encryption_sse_c_invalid_md5`, `test_encryption_sse_c_other_key`, `test_encryption_key_no_sse_c`, `test_encryption_sse_c_method_head`, `test_encryption_sse_c_multipart_upload`, `test_encryption_sse_c_multipart_bad_download`, `test_encryption_sse_c_multipart_invalid_chunks_1`, `test_encryption_sse_c_multipart_invalid_chunks_2`, `test_encryption_sse_c_unaligned_multipart_upload`, `test_encryption_sse_c_post_object_authenticated_request`, `test_encryption_sse_c_enforced_with_bucket_policy`, `test_encryption_sse_c_deny_algo_with_bucket_policy`, `test_encrypted_transfer_1b`, `test_encrypted_transfer_1kb`, `test_encrypted_transfer_1MB`, `test_encrypted_transfer_13b`, `test_get_sse_c_encrypted_object_attributes`, `test_multipart_sse_c_get_part`, `test_non_multipart_sse_c_get_part`
