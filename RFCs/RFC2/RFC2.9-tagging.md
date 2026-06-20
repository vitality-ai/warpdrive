# RFC 2.9: Tagging

**Status:** Planned  
**Parent RFC:** [RFC 2 Overview](RFC2-s3-compatibility-plan.md)  
**Date:** June 2026

---

## Priority

Medium-Low. Tagging is used for cost allocation, lifecycle rule targeting, and workflow automation. Implementation is straightforward key-value storage.

## Schema Change

```sql
CREATE TABLE bucket_tags (
    bucket TEXT NOT NULL,
    tag_key TEXT NOT NULL,
    tag_value TEXT NOT NULL,
    PRIMARY KEY (bucket, tag_key)
);

CREATE TABLE object_tags (
    user_id TEXT NOT NULL,
    bucket TEXT NOT NULL,
    key TEXT NOT NULL,
    tag_key TEXT NOT NULL,
    tag_value TEXT NOT NULL,
    PRIMARY KEY (user_id, bucket, key, tag_key)
);
```

## Changes Required

- `PUT /s3/{bucket}?tagging` — replace all bucket tags with XML body `<Tagging><TagSet><Tag><Key>k</Key><Value>v</Value></Tag></TagSet></Tagging>`
- `GET /s3/{bucket}?tagging` — return tags XML; `404 NoSuchTagSet` if no tags set
- `DELETE /s3/{bucket}?tagging` — remove all tags
- Same three routes for `PUT/GET/DELETE /s3/{bucket}/{key}?tagging`
- `GET /s3/{bucket}/{key}` — include `x-amz-tagging-count` header when object has tags
- Enforce tag limits: max 10 tags per bucket/object; key max 128 chars; value max 256 chars; return `400 BadRequest` on excess
- `PUT /s3/{bucket}/{key}?tagging` with `x-amz-tagging` header on PUT — store inline tags
- On multipart uploads: support `x-amz-tagging` on CreateMultipartUpload
- Anonymous access respects ACL: public-read objects allow tag reads (gated by ACL batch)

## Ceph Tests Targeted

`test_set_bucket_tagging`, `test_get_obj_tagging`, `test_get_obj_head_tagging`, `test_put_delete_tags`, `test_put_excess_key_tags`, `test_put_excess_tags`, `test_put_excess_val_tags`, `test_put_max_kvsize_tags`, `test_put_max_tags`, `test_put_modify_tags`, `test_put_obj_with_tags`, `test_set_multipart_tagging`, `test_delete_tags_obj_public`, `test_get_tags_acl_public`, `test_put_tags_acl_public`, and all bucket/object tagging variant tests
