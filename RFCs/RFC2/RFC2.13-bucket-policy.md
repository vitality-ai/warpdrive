# RFC 2.13: Bucket Policy

**Status:** Planned  
**Parent RFC:** [RFC 2 Overview](RFC2-s3-compatibility-plan.md)  
**Date:** June 2026

---

## Priority

Medium. Bucket policies allow fine-grained access control via JSON IAM-style policy documents. With a single admin user, the priority is lower, but several Ceph tests require basic policy support (allow/deny principals, condition operators, policy status).

## Schema Change

```sql
CREATE TABLE bucket_policy (
    bucket TEXT PRIMARY KEY,
    policy_json TEXT NOT NULL
);
```

## Design

Policy evaluation order: explicit Deny > explicit Allow > implicit Deny. For the single admin user, admin always has implicit Allow on their own buckets regardless of policy. Policy evaluation applies primarily to anonymous requests and (when Vitality Console is connected) non-admin users.

## Changes Required

- `PUT /s3/{bucket}?policy` — store JSON policy document; validate JSON structure; `400 MalformedPolicy` on bad JSON
- `GET /s3/{bucket}?policy` — return stored policy; `404 NoSuchBucketPolicy` if not set
- `DELETE /s3/{bucket}?policy` — remove policy
- Policy evaluation engine: parse `Effect`, `Principal`, `Action`, `Resource`, `Condition` fields
- Supported condition operators: `StringEquals`, `StringLike`, `StringNotEquals`, `ArnLike`, `IpAddress`, `Bool`, `*IfExists` variants (e.g., `StringEqualsIfExists`)
- `NotPrincipal` support: allow all except listed principals
- Policy-gated operations: `s3:GetObject`, `s3:PutObject`, `s3:DeleteObject`, `s3:ListBucket`, `s3:GetBucketAcl`, `s3:PutBucketAcl`, `s3:GetObjectTagging`, `s3:PutObjectTagging`, `s3:PutObjectAcl`, `s3:GetObjectAcl`, `s3:PutBucketPolicy`, `s3:GetBucketPolicy`, `s3:DeleteBucketPolicy`, `s3:ListBucketMultipartUploads`, `s3:AbortMultipartUpload`
- Tag-condition policies: `s3:RequestObjectTag`, `s3:ExistingObjectTag` condition keys
- Bucket policy status: `GET /s3/{bucket}?policyStatus` — whether the policy makes the bucket public (supplement to ACL-based status in RFC 2.10)
- Cross-account policy: deny access from a different account's principal
- Deny self policy: a policy that denies the bucket owner's own access must be rejectable via Console (prevent lockout)
- `HEAD /s3/{bucket}/{key}` returns `403` with `x-amz-request-id` when policy denies access (prefix condition)
- Multipart upload policy: policy conditions checked on `s3:PutObject` at CompleteMultipartUpload time
- Upload-part-copy policy: `s3:GetObject` on source evaluated at part-copy time

## Ceph Tests Targeted

`test_set_get_del_bucket_policy`, `test_bucket_policy`, `test_bucket_policy_acl`, `test_bucket_policy_allow_notprincipal`, `test_bucket_policy_another_bucket`, `test_bucket_policy_deny_self_denied_policy`, `test_bucket_policy_deny_self_denied_policy_confirm_header`, `test_bucket_policy_different_tenant`, `test_bucket_policy_get_obj_acl_existing_tag`, `test_bucket_policy_get_obj_existing_tag`, `test_bucket_policy_get_obj_tagging_existing_tag`, `test_bucket_policy_multipart`, `test_bucket_policy_put_obj_acl`, `test_bucket_policy_put_obj_copy_source`, `test_bucket_policy_put_obj_copy_source_meta`, `test_bucket_policy_put_obj_grant`, `test_bucket_policy_put_obj_request_obj_tag`, `test_bucket_policy_put_obj_tagging_existing_tag`, `test_bucket_policy_set_condition_operator_end_with_IfExists`, `test_bucket_policy_tenanted_bucket`, `test_bucket_policy_upload_part_copy`, `test_bucketv2_policy`, `test_bucketv2_policy_acl`, `test_bucketv2_policy_another_bucket`, `test_get_nonpublicpolicy_principal_bucket_policy_status`, `test_head_object_404_with_policy_prefix`, `test_multipart_upload_on_a_bucket_with_policy`, `test_block_public_policy`, `test_block_public_policy_with_principal`, `test_get_bucket_policy_status`, `test_get_nonpublicpolicy_acl_bucket_policy_status`, `test_post_object_expired_policy`, `test_post_object_missing_policy_condition`, `test_post_object_request_missing_policy_specified_field`
