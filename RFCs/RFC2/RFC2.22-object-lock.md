# RFC 2.22: Object Lock / WORM

**Status:** Planned  
**Parent RFC:** [RFC 2 Overview](RFC2-s3-compatibility-plan.md)  
**Date:** June 2026

---

## Priority

Low. Object Lock prevents objects from being deleted or overwritten for a configurable retention period. Requires versioning (RFC 2.11) as a prerequisite. Used for compliance (WORM — Write Once Read Many) and regulatory data retention.

## Design

Object lock is configured at the bucket level (enabled at CreateBucket time) and optionally at the object level. Two protection modes: `COMPLIANCE` (cannot be shortened even by admin) and `GOVERNANCE` (admin can override with `x-amz-bypass-governance-retention: true`). Legal hold is a separate flag that prevents deletion regardless of retention period.

## Schema Change

```sql
ALTER TABLE buckets ADD COLUMN object_lock_enabled BOOLEAN NOT NULL DEFAULT false;

CREATE TABLE object_lock_config (
    bucket TEXT PRIMARY KEY,
    mode TEXT,               -- 'COMPLIANCE' or 'GOVERNANCE'
    days INTEGER,
    years INTEGER
);

CREATE TABLE object_versions_lock (
    bucket TEXT NOT NULL,
    key TEXT NOT NULL,
    version_id TEXT NOT NULL,
    mode TEXT,               -- 'COMPLIANCE' or 'GOVERNANCE'
    retain_until_date TEXT,  -- ISO 8601
    legal_hold TEXT NOT NULL DEFAULT 'OFF',  -- 'ON' or 'OFF'
    PRIMARY KEY (bucket, key, version_id)
);
```

## Changes Required

**Bucket-level object lock:**
- `CreateBucket` with `x-amz-bucket-object-lock-enabled: true` — create bucket with object lock; versioning is automatically enabled
- `PUT /s3/{bucket}?object-lock` — set default retention mode and period; `403` if object lock was not enabled at bucket creation
- `GET /s3/{bucket}?object-lock` — return current object lock configuration; `404 ObjectLockConfigurationNotFoundError` if not configured
- `test_object_lock_get_obj_lock_invalid_bucket` and `test_object_lock_put_obj_lock_invalid_bucket`: return correct error for non-object-lock-enabled buckets
- Enable object lock after bucket creation: `test_object_lock_put_obj_lock_enable_after_create` — only allowed if bucket has no objects yet
- Invalid configuration values: `test_object_lock_put_obj_lock_invalid_days/years/mode/status` → `400 MalformedXML` or `400 InvalidArgument`
- Cannot specify both Days and Years: `test_object_lock_put_obj_lock_with_days_and_years` → `400 MalformedXML`

**Object-level retention:**
- `PUT /s3/{bucket}/{key}?retention` (with `versionId`) — set retain-until-date and mode for a specific object version
- `GET /s3/{bucket}/{key}?retention` — return retention config; `404` if no retention set
- `test_object_lock_get_obj_retention_invalid_bucket`: correct error for non-lock bucket
- Retention date must be ISO 8601 format: `test_object_lock_get_obj_retention_iso8601`
- Retention override: `test_object_lock_put_obj_retention_override_default_retention` — object-level retention can override bucket default
- Extending retention (`test_object_lock_put_obj_retention_increase_period`): always allowed
- Shortening retention (`test_object_lock_put_obj_retention_shorten_period`): `403 AccessDenied` in COMPLIANCE mode; allowed with `x-amz-bypass-governance-retention` in GOVERNANCE mode
- Bypass governance: `test_object_lock_put_obj_retention_shorten_period_bypass` — verify bypass header works
- Retention with version ID: `test_object_lock_put_obj_retention_versionid`
- Invalid mode: `test_object_lock_put_obj_retention_invalid_mode` → `400`

**Object-level legal hold:**
- `PUT /s3/{bucket}/{key}?legal-hold` — set `Status: ON` or `OFF`
- `GET /s3/{bucket}/{key}?legal-hold` — return current legal hold status
- `test_object_lock_put_legal_hold_invalid_bucket`, `test_object_lock_get_legal_hold_invalid_bucket` → correct error
- `test_object_lock_put_legal_hold_invalid_status` → `400`

**Enforcement on DELETE:**
- `DELETE /s3/{bucket}/{key}?versionId=...` on a retained object: `403 AccessDenied` (COMPLIANCE), `403` without bypass (GOVERNANCE)
- `test_object_lock_delete_object_with_retention`: blocked
- `test_object_lock_delete_object_with_retention_and_marker`: creating a delete marker is allowed but deleting the retained version is not
- `test_object_lock_delete_object_with_legal_hold_on`: blocked regardless of retention period
- `test_object_lock_delete_object_with_legal_hold_off`: allowed when hold is OFF
- Multi-object delete with retention: `test_object_lock_multi_delete_object_with_retention` — returns errors for locked objects in DeleteResult
- Multipart object with retention: `test_object_lock_delete_multipart_object_with_retention`, `test_object_lock_delete_multipart_object_with_legal_hold_on`

**Mode changing:**
- `test_object_lock_changing_mode_from_compliance`: cannot change from COMPLIANCE to GOVERNANCE → `403`
- `test_object_lock_changing_mode_from_governance_with_bypass`: can change with bypass header
- `test_object_lock_changing_mode_from_governance_without_bypass`: blocked without bypass

**Versioning interaction:**
- `test_object_lock_suspend_versioning`: suspending versioning on a bucket with object lock enabled → `400 InvalidBucketState`
- `test_object_lock_uploading_obj`: uploading an object to a bucket with default retention sets retention automatically; response includes `x-amz-object-lock-mode` and `x-amz-object-lock-retain-until-date` headers
- `test_object_lock_get_obj_metadata`: HEAD returns object lock headers

## Ceph Tests Targeted

All 39 `test_object_lock_*` tests.
