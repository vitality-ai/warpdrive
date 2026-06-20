# RFC 2.2: Object Listing — Full ListObjects V1 + V2

**Status:** Planned  
**Parent RFC:** [RFC 2 Overview](RFC2-s3-compatibility-plan.md)  
**Date:** June 2026

---

## Priority

High. The Ceph suite has ~65 listing tests. Listing is what separates a working object store from a toy. Current code only supports `list-type=2`, returns `<Size>0</Size>`, and lacks any filtering.

## Changes Required

- `GET /s3/{bucket}` without `list-type=2` → **ListObjectsV1** with `marker`-based pagination
- `GET /s3/{bucket}?list-type=2` → **ListObjectsV2** with `continuation-token`, `start-after`, `fetch-owner`
- **Prefix filtering:** `prefix=foo/` returns only keys with that prefix
- **Delimiter + CommonPrefixes:** `delimiter=/` collapses key segments into virtual directories returned as `<CommonPrefixes><Prefix>...</Prefix></CommonPrefixes>`
- **MaxKeys + truncation:** default 1000; return `<IsTruncated>true</IsTruncated>` and next marker/token when limit is hit
- **`encoding-type=url`:** URL-encode all keys and prefixes in the XML response
- Real `<Size>`, `<ETag>`, `<LastModified>` per `<Contents>` entry (from new metadata columns in RFC 2.1)
- `<KeyCount>` in V2 response
- `GET /s3` (list all buckets) with `?max-buckets=N` pagination support

## Ceph Tests Targeted

All ~65 `test_bucket_list_*` and `test_bucket_listv2_*` tests, including: empty, distinct, many, delimiter variants, prefix variants, maxkeys variants, marker/continuation-token variants, encoding, unordered, fetchowner, `test_basic_key_count`, `test_bucket_list_return_data`, `test_bucket_list_return_data_versioning`, `test_bucket_list_objects_anonymous*`, `test_list_buckets_paginated`
