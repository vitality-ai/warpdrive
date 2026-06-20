# RFC 2.12: Bucket Naming Validation & Ownership Controls

**Status:** Planned  
**Parent RFC:** [RFC 2 Overview](RFC2-s3-compatibility-plan.md)  
**Date:** June 2026

---

## Priority

Medium-Low. Bucket naming rules enforce DNS compatibility. Ownership controls (BucketOwnerEnforced, BucketOwnerPreferred, ObjectWriter) determine which user becomes the object owner on cross-account writes.

## Schema Change

```sql
ALTER TABLE buckets ADD COLUMN object_ownership TEXT NOT NULL DEFAULT 'ObjectWriter';
```

## Changes Required

**Bucket naming validation** (enforced at `PUT /s3/{bucket}` — CreateBucket):
- Reject names shorter than 3 chars or longer than 63 chars → `400 InvalidBucketName`
- Reject names that are valid IPs (`192.168.1.1`) → `400 InvalidBucketName`
- Reject names starting with a digit-only segment or non-alphanumeric → `400 InvalidBucketName`
- Reject names with `..`, `-.`, `.-` sequences → `400 InvalidBucketName`
- Reject names ending with `-` → `400 InvalidBucketName`
- Accept names with hyphens, periods, and digits in valid positions
- `test_bucket_create_exists`: re-creating an existing bucket you own returns `200` (not an error)
- `test_bucket_create_exists_nonowner`: re-creating a bucket owned by a different user returns `409 BucketAlreadyExists`

**Ownership Controls** (`PUT/GET/DELETE /s3/{bucket}?ownershipControls`):
- Store `ObjectOwnership` setting per bucket (`BucketOwnerEnforced`, `BucketOwnerPreferred`, `ObjectWriter`)
- `BucketOwnerEnforced`: disables ACLs entirely; any `x-amz-acl` or grant header → `400 InvalidBucketAclWithObjectOwnership`
- `BucketOwnerPreferred`: ACLs still work; if uploader specifies `bucket-owner-full-control`, ownership transfers to bucket owner
- `ObjectWriter`: default; object owned by uploader
- `test_bucket_create_delete_bucket_ownership`: create and delete a bucket while checking ownership controls persist correctly

**Account usage & bucket stats:**
- `GET /s3/{bucket}?usage` — return `x-rgw-bytes-used` and `x-rgw-object-count` headers (Ceph extension)
- `HEAD /s3/{bucket}?read-stats=true` — return read statistics headers (Ceph extension)
- `GET /s3` — list-all-my-buckets supports `?max-buckets=N` for paginated response

## Ceph Tests Targeted

`test_bucket_create_naming_bad_ip`, `test_bucket_create_naming_bad_short_one`, `test_bucket_create_naming_bad_short_two`, `test_bucket_create_naming_bad_starts_nonalpha`, `test_bucket_create_naming_dns_dash_at_end`, `test_bucket_create_naming_dns_dash_dot`, `test_bucket_create_naming_dns_dot_dash`, `test_bucket_create_naming_dns_dot_dot`, `test_bucket_create_naming_dns_long`, `test_bucket_create_naming_dns_underscore`, `test_bucket_create_naming_good_contains_hyphen`, `test_bucket_create_naming_good_contains_period`, `test_bucket_create_naming_good_long_60`, `test_bucket_create_naming_good_long_61`, `test_bucket_create_naming_good_long_62`, `test_bucket_create_naming_good_long_63`, `test_bucket_create_naming_good_starts_alpha`, `test_bucket_create_naming_good_starts_digit`, `test_bucket_create_special_key_names`, `test_bucket_create_exists`, `test_bucket_create_exists_nonowner`, `test_bucket_recreate_not_overriding`, `test_bucket_create_delete_bucket_ownership`, `test_create_bucket_bucket_owner_enforced`, `test_create_bucket_bucket_owner_preferred`, `test_create_bucket_no_ownership_controls`, `test_create_bucket_object_writer`, `test_put_bucket_ownership_bucket_owner_enforced`, `test_put_bucket_ownership_bucket_owner_preferred`, `test_put_bucket_ownership_object_writer`, `test_account_usage`, `test_head_bucket_usage`, `test_list_buckets_paginated`
