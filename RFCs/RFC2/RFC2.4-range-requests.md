# RFC 2.4: Range Requests & Transfer Encoding

**Status:** Planned  
**Parent RFC:** [RFC 2 Overview](RFC2-s3-compatibility-plan.md)  
**Date:** June 2026

---

## Priority

Medium-High. Range GET is critical for large-file resumable downloads, video streaming, and is required for multipart object GET-by-part (RFC 2.6). `100-continue` and `aws-chunked` are what the AWS CLI and SDK emit by default.

## Changes Required

**Range GET:**
- Parse `Range: bytes=start-end` header
- Map the byte range across the extent list stored in SQLite (warpdrive's disaggregated offset-size model makes this natural — slice the extents to cover `[start, end]`)
- Return `206 Partial Content` with `Content-Range: bytes start-end/total` and `Content-Length: (end-start+1)`
- Return `416 Range Not Satisfiable` for invalid ranges (start > end, start > object size)
- Handle suffix range (`bytes=-N` — last N bytes) and skip-leading (`bytes=N-`)
- Handle empty object range request (return 416)
- `read-through` behavior: no partial byte serving on a zero-length body

**100-Continue:**
- Handle `Expect: 100-continue` header correctly — actix-web handles most of this, but the handler must not reject the header or stall

**aws-chunked transfer encoding:**
- Detect `Content-Encoding: aws-chunked` or `x-amz-content-sha256: STREAMING-AWS4-HMAC-SHA256-PAYLOAD`
- Decode the chunked body format (each chunk is prefixed with `{hex-size};chunk-signature=...\r\n`) before passing bytes to storage

## Ceph Tests Targeted

`test_100_continue`, `test_100_continue_error_retry`, `test_object_content_encoding_aws_chunked`, `test_object_write_with_chunked_transfer_encoding`, `test_ranged_request_response_code`, `test_ranged_request_invalid_range`, `test_ranged_request_empty_object`, `test_ranged_request_skip_leading_bytes_response_code`, `test_ranged_request_return_trailing_bytes_response_code`, `test_ranged_big_request_response_code`, `test_read_through`, range-based subtests within `test_multipart_get_part`
