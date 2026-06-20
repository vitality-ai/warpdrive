# Ceph S3 Test Coverage Tracker

Tests are sourced from the Ceph `s3-tests` suite. In-scope files: `test_s3.py` (760 tests) and `test_headers.py` (48 tests) = **808 total**.

**Workflow:** Before every push, run the Ceph suite against a live server and record which tests moved from failing to passing. List only newly-passing tests per entry — do not repeat tests already listed in earlier entries.

---

## How to run the suite

```bash
cd s3-tests
pip install -e .
S3TEST_CONF=s3tests.conf pytest s3tests/functional/test_s3.py s3tests/functional/test_headers.py -v 2>&1 | tee results.txt
```

Passing count:
```bash
grep -c " PASSED" results.txt
```

---

## feat/batch-1-s3-core-ops — Batch 1 complete

**Commit:** pending  
**Branch:** `feat/batch-1-s3-core-ops`  
**RFC Batch:** Batch 1 (Core CRUD + Header Validation)  
**Newly passing:** 40

### Verified Passing

#### test_s3.py (21)
```
test_bucket_notexist
test_bucketv2_notexist
test_bucket_delete_notexist
test_bucket_delete_nonempty
test_object_write_to_nonexist_bucket
test_bucket_create_delete
test_object_read_not_exist
test_object_requestid_matches_header_on_error
test_object_head_zero_bytes
test_object_write_check_etag
test_object_write_read_update_read_delete
test_object_write_file
test_bucket_head
test_bucket_head_notexist
test_buckets_create_then_list
test_buckets_list_ctime
test_object_write_cache_control
test_object_write_expires
test_object_delete_key_bucket_gone
test_bucket_head_extended
test_object_read_unreadable
```

#### test_headers.py (19)
```
test_object_create_bad_md5_invalid_short
test_object_create_bad_md5_bad
test_object_create_bad_md5_empty
test_object_create_bad_md5_none
test_object_create_bad_expect_mismatch
test_object_create_bad_expect_empty
test_object_create_bad_expect_none
test_object_create_bad_contentlength_empty
test_object_create_bad_contentlength_negative
test_object_create_bad_contenttype_invalid
test_object_create_bad_contenttype_empty
test_object_create_bad_contenttype_none
test_bucket_create_contentlength_none
test_object_acl_create_contentlength_none
test_bucket_create_bad_expect_mismatch
test_bucket_create_bad_expect_empty
test_bucket_create_bad_contentlength_empty
test_bucket_create_bad_contentlength_negative
test_bucket_create_bad_contentlength_none
```

### Intentionally excluded (29 test_headers.py tests — not counted toward 808 total)

- **21 `_aws2` tests** — AWS SigV2 is deprecated and not implemented
- **7 `fails_on_rgw` / boto3 limitation tests** — boto3 rewrites the header being tested after signing; unfixable server-side and also fail on the reference RGW implementation
- **1 ACL test** (`test_bucket_put_bad_canned_acl`) — deferred to Batch 10

**Running total: 40 / 808**

---

<!-- Template for next entry — copy and fill in before each push:

## <branch-name> — <short description>

**Commit:** `<hash>`  
**Branch:** `<branch>`  
**RFC Batch:** Batch N (<name>)  
**Newly passing:** N

### Verified Passing

```
test_foo
test_bar
...
```

**Running total: X / 808**

-->
