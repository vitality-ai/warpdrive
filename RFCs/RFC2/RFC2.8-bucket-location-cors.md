# RFC 2.8: Bucket Location & CORS

**Status:** Planned  
**Parent RFC:** [RFC 2 Overview](RFC2-s3-compatibility-plan.md)  
**Date:** June 2026

---

## Priority

Medium. CORS is required for any browser-based S3 client or web application that accesses warpdrive directly from a browser. Location is cheap.

## Schema Change

```sql
CREATE TABLE bucket_cors (
    bucket TEXT PRIMARY KEY,
    cors_xml TEXT NOT NULL
);
```

## Changes Required

**Bucket Location:**
- `GET /s3/{bucket}?location` → `<LocationConstraint>us-east-1</LocationConstraint>` (or read from a `WARPDRIVE_REGION` env var, default `us-east-1`)

**CORS:**
- `PUT /s3/{bucket}?cors` — store XML body in `bucket_cors`
- `GET /s3/{bucket}?cors` — return stored XML; `404 NoSuchCORSConfiguration` if not set
- `DELETE /s3/{bucket}?cors` — remove row
- `OPTIONS /s3/{bucket}/{key}` — preflight: match `Origin` and `Access-Control-Request-Method` against stored rules; return `Access-Control-Allow-Origin`, `Access-Control-Allow-Methods`, `Access-Control-Allow-Headers`, `Access-Control-Max-Age`; `403` if no rule matches

## Ceph Tests Targeted

`test_bucket_get_location`, `test_set_cors`, `test_cors_origin_response`, `test_cors_origin_wildcard`, `test_cors_header_option`, `test_cors_presigned_get_object`, `test_cors_presigned_put_object`
