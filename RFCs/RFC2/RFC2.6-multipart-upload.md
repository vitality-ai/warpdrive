# RFC 2.6: Multipart Upload — Full Correctness

**Status:** Planned  
**Parent RFC:** [RFC 2 Overview](RFC2-s3-compatibility-plan.md)  
**Date:** June 2026

---

## Priority

Medium. Required for any object over 5GB and preferred for large objects. The current implementation stores parts as fake keys (`{key}.part.{n}.{uploadId}`) which collide with real object keys and don't track ETag per part.

## Schema Changes

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

## Changes Required

- **CreateMultipartUpload:** insert row into `multipart_uploads`; return `UploadId`
- **UploadPart:** validate `uploadId` exists; store part data + ETag in `multipart_parts`; enforce 5MB minimum per part (except last)
- **CompleteMultipartUpload:** parse XML part list; validate ETags match stored values; validate part numbers are sequential from 1; concatenate extent lists in order; write final object metadata; clean up `multipart_uploads` and `multipart_parts` rows
- **AbortMultipartUpload:** delete all parts from `multipart_parts` and their raw storage extents; delete `multipart_uploads` row
- **ListMultipartUploads** (`GET /s3/{bucket}?uploads`): list in-progress uploads with prefix/delimiter/max-uploads filtering
- **GET by part number** (`GET /s3/{bucket}/{key}?partNumber=N`): return the bytes for that part of a completed multipart object
- **GetObjectAttributes** (`GET /s3/{bucket}/{key}?attributes`): return structured metadata including `ETag`, `StorageClass`, `ObjectSize`, and `ObjectParts` list for multipart objects
- Remove the old part-as-fake-key logic entirely

## Ceph Tests Targeted

`test_multipart_upload_empty`, `test_multipart_upload_complete_without_create`, `test_multipart_upload_small`, `test_multipart_upload`, `test_multipart_upload_multiple_sizes`, `test_multipart_upload_resend_part`, `test_multipart_upload_contents`, `test_multipart_upload_overwrite_existing_object`, `test_multipart_upload_size_too_small`, `test_multipart_upload_missing_part`, `test_multipart_upload_incorrect_etag`, `test_abort_multipart_upload`, `test_abort_multipart_upload_not_found`, `test_list_multipart_upload`, `test_list_multipart_upload_owner`, `test_multipart_get_part`, `test_multipart_single_get_part`, `test_non_multipart_get_part`, `test_multipart_copy_small`, `test_multipart_copy_multiple_sizes`, `test_multipart_copy_without_range`, `test_multipart_copy_special_names`, `test_atomic_multipart_upload_write`, `test_multipart_resend_first_finishes_last`, `test_get_multipart_object_attributes`, `test_get_paginated_multipart_object_attributes`, `test_get_single_multipart_object_attributes`
