# RFC 2.11: Versioning

**Status:** Planned  
**Parent RFC:** [RFC 2 Overview](RFC2-s3-compatibility-plan.md)  
**Date:** June 2026

---

## Priority

Low. Significant schema change. Every PUT creates a new version row rather than overwriting. Required for strong consistency guarantees and point-in-time recovery.

## Changes Required

- New `versioning_state` column per bucket (`disabled`, `enabled`, `suspended`)
- Versioning-aware object table: all writes produce a new version with a UUID `VersionId`; reads without version ID return the latest
- Delete with versioning enabled creates a delete marker (no data deleted)
- `GET /s3/{bucket}/{key}?versionId=...` — retrieve specific version
- `DELETE /s3/{bucket}/{key}?versionId=...` — permanently delete a specific version
- `GET /s3/{bucket}?versions` — ListObjectVersions with marker, prefix, delimiter pagination
- `PUT /s3/{bucket}?versioning` — enable/suspend versioning
- Multi-object delete with versioning: delete markers created per key; `VersionId` in response
- CopyObject with versioning: `x-amz-copy-source-version-id` to copy a specific version; destination receives new VersionId
- Multipart upload returns VersionId in CompleteMultipartUpload response when versioning is enabled
- Atomic upload: PUT to a versioned bucket must return VersionId immediately, visible on GET with no race
- Concurrent operations: concurrent PUTs to the same key each produce a distinct VersionId; concurrent deletes each produce a distinct delete-marker VersionId
- Versioned ACL: `GET/PUT ?acl` on a versioned object must work without specifying a versionId (operates on latest)
- Special key names with versioning: keys that look like version IDs or have spaces/special chars still work
- Suspended versioning: new writes get `VersionId=null`; null-version object treated as the "current" version
- Delete markers on non-versioned bucket: `DELETE` on a key that never existed returns 204 with no delete marker
- Delete marker expiration (Lifecycle rule `ExpiredObjectDeleteMarker` — implemented in RFC 2.20, but the marker creation is here)

## Ceph Tests Targeted

`test_versioning_bucket_create_suspend`, `test_versioning_obj_create_read_remove`, `test_versioning_obj_create_read_remove_head`, `test_versioning_stack_delete_merkers`, `test_versioning_obj_plain_null_version_*`, `test_versioning_obj_suspend_versions`, `test_versioning_obj_create_versions_remove_all`, `test_versioning_concurrent_multi_object_delete`, `test_versioning_multi_object_delete`, `test_versioning_multi_object_delete_with_marker`, `test_versioning_multi_object_delete_with_marker_create`, `test_versioning_copy_obj_version`, `test_versioning_obj_list_marker`, `test_versioning_obj_create_overwrite_multipart`, `test_versioning_obj_create_versions_remove_special_names`, `test_versioning_obj_suspended_copy`, `test_versioning_bucket_atomic_upload_return_version_id`, `test_versioning_bucket_multipart_upload_return_version_id`, `test_versioned_concurrent_object_create_and_remove`, `test_versioned_concurrent_object_create_concurrent_remove`, `test_versioned_object_acl`, `test_versioned_object_acl_no_version_specified`, `test_object_copy_versioned_bucket`, `test_object_copy_versioned_url_encoding`, `test_object_copy_versioning_multipart_upload`, `test_multipart_copy_versioned`, `test_delete_marker_nonversioned`, `test_delete_marker_versioned`, `test_delete_marker_expiration`, `test_delete_marker_suspended`, `test_get_versioned_object_attributes`
