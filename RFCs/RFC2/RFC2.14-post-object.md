# RFC 2.14: POST Object (HTML Form Upload)

**Status:** Planned  
**Parent RFC:** [RFC 2 Overview](RFC2-s3-compatibility-plan.md)  
**Date:** June 2026

---

## Priority

Medium-Low. POST-based upload allows browser-native file upload via HTML form without exposing credentials. It uses a policy document signed by the server to authorize specific form fields.

## Design

`POST /s3/{bucket}` with `multipart/form-data` encoding. The form contains a `key`, `policy` (base64 JSON), `x-amz-signature`, and optional fields. The server validates the signature and policy, then stores the object.

## Changes Required

- Route: `POST /s3/{bucket}` — parse `multipart/form-data` body
- Extract `key`, `Content-Type`, `policy`, `x-amz-algorithm`, `x-amz-credential`, `x-amz-date`, `x-amz-signature`, `x-amz-meta-*`, `tagging`, `acl`, `success_action_redirect`, `success_action_status` fields
- Policy validation: base64-decode → JSON parse `{"expiration": "...", "conditions": [...]}`; check `expiration` not past; match each condition against the form fields
- Condition types: `["eq", "$field", "value"]`, `["starts-with", "$field", "prefix"]`, `["content-length-range", min, max]`; conditions are case-insensitive on field names
- Signature verification: HMAC-SHA256 over the raw base64 policy string using SigV4 signing key
- On success: return `204` (or `200`/redirect per `success_action_status`/`success_action_redirect`)
- On error: return S3 XML error (not HTML) — `400 InvalidArgument`, `403 SignatureDoesNotMatch`, `403 ExpiredToken`
- Object size limits: `content-length-range` condition enforced; `413 EntityTooLarge` on excess
- Tags: `tagging` form field accepted as URL-encoded `key=value&key2=value2` pairs
- Anonymous POST: no signature — allowed if bucket policy or ACL grants public-write
- Checksum: `x-amz-checksum-*` in form field → verify and store

## Ceph Tests Targeted

`test_post_object_anonymous_request`, `test_post_object_authenticated_no_content_type`, `test_post_object_authenticated_request`, `test_post_object_authenticated_request_bad_access_key`, `test_post_object_case_insensitive_condition_fields`, `test_post_object_condition_is_case_sensitive`, `test_post_object_empty_conditions`, `test_post_object_escaped_field_values`, `test_post_object_expires_is_case_sensitive`, `test_post_object_ignored_header`, `test_post_object_invalid_access_key`, `test_post_object_invalid_content_length_argument`, `test_post_object_invalid_date_format`, `test_post_object_invalid_request_field_value`, `test_post_object_invalid_signature`, `test_post_object_missing_conditions_list`, `test_post_object_missing_content_length_argument`, `test_post_object_missing_expires_condition`, `test_post_object_missing_signature`, `test_post_object_no_key_specified`, `test_post_object_set_invalid_success_code`, `test_post_object_set_key_from_filename`, `test_post_object_set_success_code`, `test_post_object_success_redirect_action`, `test_post_object_tags_anonymous_request`, `test_post_object_tags_authenticated_request`, `test_post_object_upload_checksum`, `test_post_object_upload_larger_than_chunk`, `test_post_object_upload_size_below_minimum`, `test_post_object_upload_size_limit_exceeded`, `test_post_object_upload_size_rgw_chunk_size_bug`, `test_post_object_user_specified_header`, `test_post_object_wrong_bucket`
