# RFC 2.19: Atomicity & Concurrency

**Status:** Planned  
**Parent RFC:** [RFC 2 Overview](RFC2-s3-compatibility-plan.md)  
**Date:** June 2026

---

## Priority

Medium. Concurrent writes to the same key must be serializable — readers must see either the full old object or the full new object, never a torn write. These tests verify that warpdrive's haystack extent model and SQLite metadata writes are atomic from the client's perspective.

## Design

SQLite's WAL mode and `BEGIN IMMEDIATE` transactions provide serialization for metadata. The haystack append-only design means bytes are written before the metadata row is updated — atomicity is achieved by writing extents to the store first, then committing the metadata row in a single transaction.

## Changes Required

- `PUT` handler: write all bytes to haystack, then commit the metadata row (etag, size, key, last_modified) in a single `BEGIN IMMEDIATE` transaction; if the transaction fails, the orphaned bytes are cleaned up on next compaction
- Concurrent PUT to the same key: second writer blocks on the SQLite write lock; when it succeeds, its object is fully visible; the first writer's object is fully replaced — no partial state visible to a reader at any point
- Dual concurrent writes (`test_atomic_dual_write_*`): two writers to the same key; the reader that runs after both complete sees exactly one complete version
- Conditional write: `If-None-Match: *` under concurrency — at most one writer wins; the other gets `412`; validated for 1MB, dual-conditional, and race conditions
- GET during concurrent PUT: reader must not see a partial object — either the old ETag+size or the new one, never a mix
- Bucket-gone atomicity: a write that lands after the bucket is deleted returns `404 NoSuchBucket` immediately; no partial data persisted

## Ceph Tests Targeted

`test_atomic_write_1mb`, `test_atomic_write_4mb`, `test_atomic_write_8mb`, `test_atomic_read_1mb`, `test_atomic_read_4mb`, `test_atomic_read_8mb`, `test_atomic_dual_write_1mb`, `test_atomic_dual_write_4mb`, `test_atomic_dual_write_8mb`, `test_atomic_write_bucket_gone`, `test_atomic_conditional_write_1mb`, `test_atomic_dual_conditional_write_1mb`
