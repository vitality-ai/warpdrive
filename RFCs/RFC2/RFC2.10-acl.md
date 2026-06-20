# RFC 2.10: ACL — Canned ACLs, Block Public Access

**Status:** Planned  
**Parent RFC:** [RFC 2 Overview](RFC2-s3-compatibility-plan.md)  
**Date:** June 2026

---

## Priority

Medium-Low. With a single admin user, full per-user ACL evaluation is not needed yet. Implement canned ACLs to support public-read buckets/objects and anonymous access patterns. Also implement Block Public Access controls.

## Design

ACL is hardcoded for the admin user: admin always has full access to everything they own. ACL only gates unauthenticated (anonymous) access to bucket/object resources. No per-user grant table needed at this stage.

## Schema Change

```sql
ALTER TABLE objects ADD COLUMN acl TEXT DEFAULT 'private';

CREATE TABLE bucket_acl (
    bucket TEXT PRIMARY KEY,
    acl TEXT NOT NULL DEFAULT 'private'
);

CREATE TABLE public_access_block (
    bucket TEXT PRIMARY KEY,
    block_public_acls BOOLEAN NOT NULL DEFAULT false,
    ignore_public_acls BOOLEAN NOT NULL DEFAULT false,
    block_public_policy BOOLEAN NOT NULL DEFAULT false,
    restrict_public_buckets BOOLEAN NOT NULL DEFAULT false
);
```

## Changes Required

- On PUT (object or bucket): parse `x-amz-acl` header; store canned ACL value (`private`, `public-read`, `public-read-write`, `authenticated-read`)
- On anonymous GET (no `Authorization` header): check stored ACL; allow if `public-read` or `public-read-write`; return `403 AccessDenied` if `private`
- `PUT /s3/{bucket}?acl` — update bucket ACL
- `GET /s3/{bucket}?acl` — return bucket ACL as XML `<AccessControlPolicy>` with owner and grant list
- `PUT /s3/{bucket}/{key}?acl` — update object ACL
- `GET /s3/{bucket}/{key}?acl` — return object ACL XML
- Concurrent ACL set: `x-amz-acl` header on bucket PUT and `PUT ?acl` must be consistent under concurrent operations
- Header-based grant ACLs: `x-amz-grant-read`, `x-amz-grant-write`, `x-amz-grant-read-acp`, `x-amz-grant-full-control` headers (group grants including `AllUsers` URI)
- Object ACL timestamp: `Last-Modified` on object must update when ACL changes
- List-buckets anonymous: `403` (S3 never allows anonymous list-all-my-buckets)
- **Block Public Access** (`PUT/GET/DELETE /s3/{bucket}?publicAccessBlock`): store four flags per bucket; when `blockPublicAcls=true` reject public-read/write canned ACLs on PUT; when `ignorePublicAcls=true` treat all ACLs as private on read; when `blockPublicPolicy=true` reject bucket policy that grants public access; when `restrictPublicBuckets=true` block all public and cross-account access
- `GET /s3/{bucket}?policyStatus` — return `<PolicyStatus><IsPublic>...</IsPublic></PolicyStatus>`

## Ceph Tests Targeted

`test_object_anon_put`, `test_object_anon_put_write_access`, `test_object_put_authenticated`, `test_access_bucket_private_object_private`, `test_access_bucket_private_object_publicread`, `test_access_bucket_private_object_publicreadwrite`, `test_access_bucket_private_objectv2_private`, `test_access_bucket_private_objectv2_publicread`, `test_access_bucket_private_objectv2_publicreadwrite`, `test_access_bucket_publicread_object_private`, `test_access_bucket_publicread_object_publicread`, `test_access_bucket_publicread_object_publicreadwrite`, `test_access_bucket_publicreadwrite_*`, `test_bucket_acl_*`, `test_object_acl_*`, `test_list_buckets_anonymous`, `test_list_buckets_invalid_auth`, `test_list_buckets_bad_auth`, `test_bucket_concurrent_set_canned_acl`, `test_bucket_header_acl_grants`, `test_bucket_recreate_new_acl`, `test_bucket_recreate_overwrite_acl`, `test_object_copy_canned_acl`, `test_object_header_acl_grants`, `test_object_put_acl_mtime`, `test_object_raw_authenticated_bucket_acl`, `test_object_raw_authenticated_object_acl`, `test_object_raw_get_bucket_acl`, `test_object_raw_get_object_acl`, `test_put_bucket_acl_grant_group_read`, `test_object_presigned_put_object_with_acl_tenant`, `test_cors_presigned_put_object_with_acl`, `test_put_get_delete_public_block`, `test_put_public_block`, `test_get_undefined_public_block`, `test_block_public_object_canned_acls`, `test_block_public_put_bucket_acls`, `test_block_public_restrict_public_buckets`, `test_ignore_public_acls`, `test_get_authpublic_acl_bucket_policy_status`, `test_get_nonpublicpolicy_acl_bucket_policy_status`, `test_get_public_acl_bucket_policy_status`, `test_get_publicpolicy_acl_bucket_policy_status`, `test_get_public_block_deny_bucket_policy`
