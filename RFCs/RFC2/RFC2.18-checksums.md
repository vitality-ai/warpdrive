# RFC 2.18: Checksums & Object Attributes

**Status:** Planned  
**Parent RFC:** [RFC 2 Overview](RFC2-s3-compatibility-plan.md)  
**Date:** June 2026

---

## Priority

Medium-Low. Additional checksum algorithms (CRC32, CRC32C, SHA-1, SHA-256, CRC64NVME) and the `GetObjectAttributes` API are required for integrity verification and SDK-level object inspection without a full GET.

## Schema Change

```sql
ALTER TABLE haystack ADD COLUMN checksum_algorithm TEXT;  -- 'CRC32', 'CRC32C', 'SHA1', 'SHA256', 'CRC64NVME'
ALTER TABLE haystack ADD COLUMN checksum_value TEXT;
```

## Changes Required

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

## Ceph Tests Targeted

`test_object_checksum_sha256`, `test_object_checksum_crc64nvme`, `test_multipart_checksum_sha256`, `test_multipart_use_cksum_helper_crc32`, `test_multipart_use_cksum_helper_crc32c`, `test_multipart_use_cksum_helper_crc64nvme`, `test_multipart_use_cksum_helper_sha1`, `test_multipart_use_cksum_helper_sha256`, `test_get_object_attributes`, `test_get_checksum_object_attributes`, `test_get_multipart_checksum_object_attributes`, `test_get_multipart_object_attributes`, `test_get_paginated_multipart_object_attributes`, `test_get_single_multipart_object_attributes`, `test_get_versioned_object_attributes`, `test_get_sse_c_encrypted_object_attributes`, `test_multipart_reupload_checksum_and_etag`
