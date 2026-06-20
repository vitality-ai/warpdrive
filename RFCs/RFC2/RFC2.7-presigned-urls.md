# RFC 2.7: Presigned URLs

**Status:** Planned  
**Parent RFC:** [RFC 2 Overview](RFC2-s3-compatibility-plan.md)  
**Date:** June 2026

---

## Priority

Medium. Required for browser direct-upload flows, temporary access grants, and any use case where S3 credentials cannot be embedded in the client.

## Design

Presigned URLs move the SigV4 auth from the `Authorization` header into query parameters. No new storage is needed — this is pure auth logic in `src/s3/auth.rs`.

## Changes Required

- Detect presigned request: check for `X-Amz-Algorithm` query param (instead of `Authorization` header)
- Parse `X-Amz-Credential`, `X-Amz-Date`, `X-Amz-Expires`, `X-Amz-SignedHeaders`, `X-Amz-Signature` from query string
- Verify expiry: reject if `X-Amz-Date` (parsed as timestamp) + `X-Amz-Expires` (seconds) < current time; return `403 RequestExpired`
- Run the same SigV4 canonical request computation with query-string params instead of header params
- Allow anonymous bucket/object access for correctly-signed presigned GETs (bypass ACL check for the request's specific resource)
- Support SigV2 presigned format (query params `AWSAccessKeyId`, `Expires`, `Signature`) — several `_aws2` tests exercise this path
- Tenant-qualified access keys (key format `tenant$user`) must be resolved against both admin bypass and Vitality Console lookup
- `X-Amz-Algorithm=AWS4-HMAC-SHA256` (V4) and plain V2 presigned URLs must both work

## Ceph Tests Targeted

`test_object_raw_get`, `test_object_raw_get_bucket_gone`, `test_object_raw_get_object_gone`, `test_object_raw_authenticated`, `test_object_raw_authenticated_bucket_gone`, `test_object_raw_authenticated_object_gone`, `test_object_raw_get_x_amz_expires_not_expired`, `test_object_raw_get_x_amz_expires_out_range_zero`, `test_object_raw_get_x_amz_expires_out_max_range`, `test_object_raw_get_x_amz_expires_out_positive_range`, `test_object_raw_put_authenticated_expired`, `test_object_presigned_put_object_with_acl`, `test_object_raw_response_headers`, `test_object_raw_get_x_amz_expires_not_expired_tenant`, `test_cors_presigned_get_object_v2`, `test_cors_presigned_put_object_v2`, `test_cors_presigned_get_object_tenant`, `test_cors_presigned_get_object_tenant_v2`, `test_cors_presigned_put_object_tenant`, `test_cors_presigned_put_object_tenant_v2`, `test_cors_presigned_put_object_tenant_with_acl`
