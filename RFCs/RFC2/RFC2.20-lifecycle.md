# RFC 2.20: Lifecycle Rules

**Status:** Planned  
**Parent RFC:** [RFC 2 Overview](RFC2-s3-compatibility-plan.md)  
**Date:** June 2026

---

## Priority

Low. Lifecycle rules automate object expiration, multipart cleanup, version transitions, and delete-marker removal. Requires a background scheduler that runs lifecycle evaluation at configurable intervals.

## Schema Change

```sql
CREATE TABLE bucket_lifecycle (
    bucket TEXT PRIMARY KEY,
    lifecycle_json TEXT NOT NULL  -- serialized XML → JSON for easy querying
);
```

## Design

A background task (tokio task or separate thread) runs every configurable interval (default 1 hour, configurable via `WARPDRIVE_LIFECYCLE_INTERVAL_SECS`). It iterates all buckets with a lifecycle config, evaluates rules against current object metadata, and performs deletions/transitions in SQLite transactions.

## Changes Required

**Lifecycle configuration:**
- `PUT /s3/{bucket}?lifecycle` — store lifecycle XML; validate rules (each rule needs ID, Status, Filter, at least one action)
- `GET /s3/{bucket}?lifecycle` — return stored config; `404 NoSuchLifecycleConfiguration` if not set
- `DELETE /s3/{bucket}?lifecycle` — remove config
- Rule ID uniqueness enforced; `400 InvalidArgument` on duplicate IDs or IDs > 255 chars
- Rule statuses: `Enabled` / `Disabled`
- Invalid date format in `Expiration`: `400 InvalidArgument`

**Expiration actions:**
- `Expiration.Days`: delete objects older than N days
- `Expiration.Date`: delete objects created before a specific date
- `Expiration.ExpiredObjectDeleteMarker`: delete orphaned delete markers (all versions deleted)
- `NoncurrentVersionExpiration.NoncurrentDays`: delete noncurrent versions older than N days
- `NoncurrentVersionExpiration.NewerNoncurrentVersions`: keep only the N most recent noncurrent versions, expire the rest
- Filter by `Prefix`, `Tag` (key+value pair), or `ObjectSize` (gt/lt filters)
- `Expiration.Days=0` is valid (expire objects immediately on the next evaluation cycle)

**Transition actions:**
- `Transition` rule moves objects to a different storage class after N days (for Warpdrive's initial impl: treat as a no-op or log; actual tiering is out of scope unless cloud storage class is configured)
- `NoncurrentVersionTransition`: same for noncurrent versions
- Cloud transition (`test_lifecycle_cloud_*`): requires `[s3 cloud]` config section; skip if not configured

**Expiration headers:**
- `x-amz-expiration` header on GET/HEAD responses when the object matches an active lifecycle expiration rule: `expiry-date="...", rule-id="..."` format

**Multipart expiration:**
- `AbortIncompleteMultipartUpload.DaysAfterInitiation`: abort in-progress uploads older than N days

## Ceph Tests Targeted

`test_lifecycle_set`, `test_lifecycle_get`, `test_lifecycle_get_no_id`, `test_lifecycle_id_too_long`, `test_lifecycle_same_id`, `test_lifecycle_invalid_status`, `test_lifecycle_set_date`, `test_lifecycle_set_invalid_date`, `test_lifecycle_set_filter`, `test_lifecycle_set_empty_filter`, `test_lifecycle_set_deletemarker`, `test_lifecycle_set_noncurrent`, `test_lifecycle_set_noncurrent_transition`, `test_lifecycle_set_multipart`, `test_lifecycle_delete`, `test_lifecycle_expiration`, `test_lifecycle_expiration_date`, `test_lifecycle_expiration_days0`, `test_lifecycle_expiration_header_head`, `test_lifecycle_expiration_header_put`, `test_lifecycle_expiration_header_and_tags_head`, `test_lifecycle_expiration_header_tags_head`, `test_lifecycle_expiration_tags1`, `test_lifecycle_expiration_tags2`, `test_lifecycle_expiration_versioned_tags2`, `test_lifecycle_expiration_versioning_enabled`, `test_lifecycle_expiration_newer_noncurrent`, `test_lifecycle_expiration_noncur_tags1`, `test_lifecycle_expiration_size_gt`, `test_lifecycle_expiration_size_lt`, `test_lifecycle_noncur_expiration`, `test_lifecycle_deletemarker_expiration`, `test_lifecycle_deletemarker_expiration_with_days_tag`, `test_lifecycle_multipart_expiration`, `test_lifecyclev2_expiration`, `test_lifecycle_transition`, `test_lifecycle_transition_set_invalid_date`, `test_lifecycle_transition_single_rule_multi_trans`, `test_lifecycle_transition_encrypted`, `test_lifecycle_noncur_transition`, `test_lifecycle_plain_null_version_current_transition`, `test_lifecycle_cloud_transition`, `test_lifecycle_cloud_transition_large_obj`, `test_lifecycle_cloud_multiple_transition`, `test_lifecycle_cloud_transition_target_by_bucket`, `test_lifecycle_cloud_transition_target_by_bucket_multiple_buckets`, `test_lifecycle_noncur_cloud_transition`, `test_delete_marker_expiration`, `test_delete_marker_suspended`
