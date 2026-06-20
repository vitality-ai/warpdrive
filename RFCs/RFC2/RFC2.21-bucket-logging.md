# RFC 2.21: Bucket Logging

**Status:** Planned  
**Parent RFC:** [RFC 2 Overview](RFC2-s3-compatibility-plan.md)  
**Date:** June 2026

---

## Priority

Low. Bucket access logging records every request to a target bucket as log objects with a configurable prefix. The test suite has 113 tests covering configuration, flush modes (JSON/simple), concurrent updates, cleanup, and event types.

## Schema Change

```sql
CREATE TABLE bucket_logging (
    bucket TEXT PRIMARY KEY,
    target_bucket TEXT NOT NULL,
    target_prefix TEXT NOT NULL DEFAULT '',
    logging_config_json TEXT NOT NULL  -- full XML → JSON for querying fields
);
```

## Design

Log records are appended to an in-memory buffer per bucket. A background flush task writes them to the target bucket as objects with the configured prefix at regular intervals (or immediately on `?logging=flush` Ceph extension). Two log formats: simple (space-separated fields, S3 standard) and JSON (`j` suffix tests). Log object keys are timestamped and unique.

## Changes Required

**Configuration:**
- `PUT /s3/{bucket}?logging` — store logging target bucket and prefix; validate target bucket exists; `400` if target bucket not found
- `GET /s3/{bucket}?logging` — return logging config XML (or empty `<BucketLoggingStatus/>` if disabled)
- `DELETE /s3/{bucket}?logging` (via PUT with empty XML) — disable logging
- Log target must have `FULL_CONTROL` grant to the log delivery group (`http://acs.amazonaws.com/groups/s3/LogDelivery`) on its ACL — `test_bucket_logging_bucket_acl_required` and `test_bucket_logging_object_acl_required` verify this
- `test_bucket_logging_bucket_auth_type`: logging requires authenticated PUT (anonymous PUT not allowed for logging config)

**Log delivery:**
- Each S3 API request to a logged bucket produces one log record containing: bucket name, time, remote IP, requester, key, operation, HTTP status, bytes sent, object size, request ID, host ID
- Log records are buffered and flushed to the target bucket; log objects have keys like `{prefix}{timestamp}-{uniqueid}`
- Flush extensions (Ceph-specific): `POST /s3/{bucket}?logging=flush` triggers immediate flush; response returns number of records flushed
- Single-log-object mode: `?logging=flush&single=true` — all buffered records in one object instead of one per request (used by `_single` suffix tests)
- JSON format (`j` suffix tests): log in JSON Lines format; one JSON object per line
- Simple format (`s` suffix tests): space-separated fields, S3 standard log format

**Log content operations:**
- `test_bucket_logging_put_objects`, `test_bucket_logging_get_objects`, `test_bucket_logging_head_objects`, `test_bucket_logging_delete_objects`: verify that PUT, GET, HEAD, DELETE operations each produce a correct log record in the target bucket
- `test_bucket_logging_copy_objects`, `test_bucket_logging_mpu_*`: CopyObject and multipart operations produce log records
- `test_bucket_logging_multi_delete`: multi-object delete produces per-key log records
- Versioned variants: all above with versioning enabled; VersionId appears in log record

**Configuration management:**
- `test_bucket_logging_mtime`: logging config PUT updates the bucket's last-modified time
- `test_bucket_logging_notupdating_*`: disabling logging (PUT with empty config) does not produce further log records
- `test_bucket_logging_owner`: log records include the correct bucket owner
- `test_bucket_logging_multiple_prefixes`: multiple logging configs can target different prefixes in the same target bucket
- `test_bucket_logging_single_prefix`, `test_bucket_logging_partitioned_key`, `test_bucket_logging_simple_key`: prefix format variants
- `test_bucket_logging_roll_time`: log object roll-over time is configurable
- Key filter: `test_bucket_logging_key_filter_*` — log only requests whose key matches a prefix filter

**Concurrent operations:**
- `test_bucket_logging_concurrent_flush_*`: concurrent flush requests do not produce duplicate records
- `test_bucket_logging_put_concurrency`: concurrent PUTs each produce exactly one log record
- `test_bucket_logging_conf_concurrent_updating_*`: concurrent config updates do not lose records or produce partial configs

**Cleanup (cascade delete):**
- `test_bucket_logging_cleanup_bucket_deletion_*`: deleting the source bucket disables logging and cleans up config
- `test_bucket_logging_cleanup_bucket_concurrent_deletion_*`: concurrent bucket deletion safe
- `test_bucket_logging_target_cleanup_*`: deleting the target bucket causes logging to become inactive gracefully
- `test_bucket_logging_part_cleanup_*`: partial log records (from aborted requests) are not written

**Extensions and permissions:**
- `test_bucket_logging_extensions`: Ceph-specific extensions to the logging config XML
- `test_bucket_logging_permissions`: checks that the log delivery ACL grant is required
- `test_put_bucket_logging`, `test_put_bucket_logging_errors`, `test_put_bucket_logging_permissions`, `test_put_bucket_logging_policy_wildcard`, `test_put_bucket_logging_policy_wildcard_objects`, `test_put_bucket_logging_extensions`: config PUT variants
- `test_put_bucket_logging_account_j`, `test_put_bucket_logging_account_s`, `test_put_bucket_logging_tenant_j`, `test_put_bucket_logging_tenant_s`: cross-account and cross-tenant logging
- `test_rm_bucket_logging`: explicit remove logging config
- `test_bucket_logging_requester_assumed_role`: log record requester field when request is made via IAM assumed role — **depends on Vitality Console / STS integration**

**Event types:**
- `test_bucket_logging_event_type_j/s`: log records include an event type field (e.g., `REST.PUT.OBJECT`, `REST.GET.OBJECT`)

## Ceph Tests Targeted

All 113 `test_bucket_logging_*`, `test_put_bucket_logging*`, and `test_rm_bucket_logging` tests.
