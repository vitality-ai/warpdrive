# RFC 2: S3 Compatibility Plan for Warpdrive

**Status:** Draft  
**Date:** June 2026  
**Authors:** Tejas-ChandraShekarRaju

---

## Abstract

This RFC defines the phased plan for achieving S3 API compatibility in Warpdrive. Tests are sourced from the Ceph S3 test suite (`s3-tests`) and organized into sub-RFCs by priority — starting with the operations used by virtually every S3 client and progressing toward advanced features. Each sub-RFC identifies what currently exists, what is broken or missing, and which Ceph tests it unlocks.

**Coverage goal:** all tests in `test_s3.py` (760) and `test_headers.py` (48), totalling 808 in-scope tests. The files `test_iam.py`, `test_sts.py`, `test_sns.py`, `test_s3select.py`, and `test_s3control.py` are explicitly out of scope (IAM/STS live in Vitality Console; SNS, S3 Select, and S3 Control are separate services).

---

## Background

Warpdrive is a custom object storage system built for disaggregated architectures. The server exposes an S3-compatible API at `/s3/...` backed by a local haystack storage engine and SQLite metadata store. The Ceph `s3-tests` suite is used as the S3 conformance benchmark.

### Current State Summary

| Area | Status |
|---|---|
| SigV4 authentication | Works, but **requires Vitality Console** (no standalone mode) |
| PUT / GET / DELETE Object | Partial — broken status codes, placeholder ETag, no metadata stored |
| HEAD Object | Stub — always returns 200 even if object doesn't exist |
| ListObjects | V2 only, returns `<Size>0</Size>`, no ETag or LastModified |
| CreateBucket | No-op — not persisted |
| DeleteBucket | Route missing |
| Bucket existence check | Not enforced |
| Error responses | Plain text — should be S3 XML |
| Multipart upload | Basic — part keys collide with real object keys, no state table |
| CopyObject | Has cross-bucket bug, placeholder ETag |
| Multi-object delete | Not implemented |
| Conditional requests | Not implemented |
| Range GET | Not implemented |
| Presigned URLs | Not implemented |
| CORS | Not implemented |
| Tagging | Not implemented |
| ACL | Not implemented |
| Versioning | Not implemented |

### IAM / Auth Scope

IAM lives in **Vitality Console**, not in Warpdrive. For the purpose of this plan, Warpdrive runs with a single hardcoded admin user (see Prerequisite below). Later, when a user explicitly configures and connects Vitality Console, non-admin credentials will be routed there.

---

## Prerequisite: Admin User Mode

**Must be implemented before any sub-RFC.** All s3-tests require a working S3 endpoint with credentials. Currently auth hard-requires `VITALITY_CONSOLE_URL` and `WARPDRIVE_SERVICE_SECRET`, making standalone testing impossible.

### Design

Add two new optional env vars:

```
WARPDRIVE_ADMIN_ACCESS_KEY=adminkey
WARPDRIVE_ADMIN_SECRET_KEY=adminsecret
```

In `src/s3/auth.rs`, before consulting Vitality Console, check whether the request's access key matches `WARPDRIVE_ADMIN_ACCESS_KEY`. If it does:

- Skip all Console network calls.
- Use `WARPDRIVE_ADMIN_SECRET_KEY` as the secret for SigV4 verification.
- Set `owner_id = "admin"`.
- Set `allowed_buckets = all` (query SQLite directly so list-buckets works without a pre-registered set).

The existing Console path remains unchanged. If neither admin key nor Console is configured, authentication fails as before.

**Future hook:** If Vitality Console is configured and the user explicitly invalidates the default admin user via the Console UI, the admin access key will be rejected locally and re-routed to Console on the next request.

---

## Sub-RFC Index

| RFC | Focus | Prerequisite | Tests |
|---|---|---|---|
| **Pre** | Admin user bypass in auth.rs | — | — |
| **[RFC 2.1](RFC2.1-core-crud.md)** ✅ | Core CRUD correctness + schema migration + header validation | Pre | ~70 |
| **[RFC 2.2](RFC2.2-object-listing.md)** | ListObjects V1 + V2 full spec | 2.1 | ~66 |
| **[RFC 2.3](RFC2.3-object-properties.md)** | Object metadata + conditional GET/PUT/COPY/DELETE | 2.1 | ~49 |
| **[RFC 2.4](RFC2.4-range-requests.md)** | Range GET + ranged variants + chunked encoding | 2.1 | ~12 |
| **[RFC 2.5](RFC2.5-multi-object-delete.md)** | Multi-object delete + CopyObject fixes | 2.1 | ~21 |
| **[RFC 2.6](RFC2.6-multipart-upload.md)** | Multipart upload rewrite + object attributes | 2.1, 2.4 | ~27 |
| **[RFC 2.7](RFC2.7-presigned-urls.md)** | Presigned URLs (V4 + V2) + tenant/v2 presigned CORS | 2.1 | ~21 |
| **[RFC 2.8](RFC2.8-bucket-location-cors.md)** | Bucket location + CORS | 2.1 | ~7 |
| **[RFC 2.9](RFC2.9-tagging.md)** | Tagging (bucket + object + limits + ACL-gated) | 2.1 | ~16 |
| **[RFC 2.10](RFC2.10-acl.md)** | Canned ACLs + header grants + Block Public Access | 2.1, 2.2 | ~45 |
| **[RFC 2.11](RFC2.11-versioning.md)** | Versioning (full) + delete markers + copy/multipart versioned | 2.1, 2.2, 2.3 | ~31 |
| **[RFC 2.12](RFC2.12-bucket-naming.md)** | Bucket naming validation + ownership controls + usage stats | 2.1 | ~33 |
| **[RFC 2.13](RFC2.13-bucket-policy.md)** | Bucket policy (JSON policy engine) | 2.1, 2.2, 2.10 | ~34 |
| **[RFC 2.14](RFC2.14-post-object.md)** | POST Object (HTML form upload) | 2.1 | ~33 |
| **[RFC 2.15](RFC2.15-sse-s3.md)** | SSE-S3 (server-managed encryption) | 2.1 | ~15 |
| **[RFC 2.16](RFC2.16-sse-c.md)** | SSE-C (customer-provided keys) | 2.1 | ~22 |
| **[RFC 2.17](RFC2.17-sse-kms.md)** | SSE-KMS (KMS-managed keys) | 2.1, 2.15 | ~30 |
| **[RFC 2.18](RFC2.18-checksums.md)** | Checksums (CRC32/CRC32C/SHA/CRC64NVME) + GetObjectAttributes | 2.1, 2.6, 2.11 | ~17 |
| **[RFC 2.19](RFC2.19-atomicity.md)** | Atomicity & concurrency guarantees | 2.1 | ~12 |
| **[RFC 2.20](RFC2.20-lifecycle.md)** | Lifecycle rules (expiration + transition + multipart cleanup) | 2.1, 2.2, 2.9, 2.11 | ~49 |
| **[RFC 2.21](RFC2.21-bucket-logging.md)** | Bucket logging (access logs to target bucket) | 2.1, 2.2, 2.10, 2.11 | ~113 |
| **[RFC 2.22](RFC2.22-object-lock.md)** | Object Lock / WORM | 2.1, 2.11 | ~39 |
| **[RFC 2.23](RFC2.23-object-restore.md)** | Object Restore + torrent | 2.11, 2.20 | ~6 |

---

## Out of Scope (IAM lives in Vitality Console)

The following test files from the Ceph suite are explicitly out of scope for Warpdrive. They belong to Vitality Console or are not part of the core object storage contract:

| File | Reason |
|---|---|
| `test_iam.py` | IAM users, roles, policies — Vitality Console |
| `test_sts.py` | AssumeRole, federation tokens — Vitality Console |
| `test_sns.py` | Bucket event notifications — separate notification service |
| `test_s3select.py` | SQL-in-place query engine — out of scope |
| `test_s3control.py` | Multi-region access points, S3 Batch — out of scope |
| Replication tests | Cross-region replication — future |

**Note:** `test_bucket_logging_requester_assumed_role` (1 test within RFC 2.21) depends on IAM assumed-role context from Vitality Console. It will be skipped until Vitality Console integration is complete.
