"""pytest plugin for the S3 compatibility job: don't let cleanup failures cascade.

Before and after every test, the suite deletes every bucket left over from
earlier tests (nuke_prefixed_buckets). It tries every bucket, then re-raises
the first error. So if one bucket can't be deleted, every later test errors
in setup, and one server bug turns into hundreds of errors.

Two cases hit this on a WarpDrive cluster today:
- any bucket with an open multipart upload makes ListObjectVersions and
  ListMultipartUploads return 500
- a browser-form POST upload is stored by the single-node engine, so the
  bucket can't be emptied (BucketNotEmpty)

This plugin turns that cleanup error into a warning. Each test creates its
own new bucket, so a leftover bucket doesn't change its result. Tests still
pass or fail on their own assertions.

It also remembers each bucket that failed to delete and skips it from then
on. Otherwise the suite retries every stuck bucket before and after every
test, and as they pile up each test gets slower than the last.

Loaded with `-p s3tests_tolerant_cleanup` and this directory on PYTHONPATH.
"""
import warnings


class LeftoverBucketWarning(UserWarning):
    pass


def pytest_configure(config):
    import s3tests.functional as functional

    original_nuke_bucket = functional.nuke_bucket
    original_nuke_prefixed = functional.nuke_prefixed_buckets
    stuck = set()

    def skip_stuck_nuke_bucket(client, bucket):
        if bucket in stuck:
            return
        try:
            original_nuke_bucket(client, bucket)
        except Exception:
            stuck.add(bucket)
            raise

    def tolerant_nuke_prefixed_buckets(prefix, client=None):
        try:
            original_nuke_prefixed(prefix, client=client)
        except Exception as e:
            warnings.warn(f"cleanup couldn't delete a leftover bucket: {e}", LeftoverBucketWarning)

    # The suite looks both names up in its module at call time.
    functional.nuke_bucket = skip_stuck_nuke_bucket
    functional.nuke_prefixed_buckets = tolerant_nuke_prefixed_buckets
