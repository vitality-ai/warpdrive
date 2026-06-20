# RFC 2.23: Object Restore & Miscellaneous

**Status:** Planned  
**Parent RFC:** [RFC 2 Overview](RFC2-s3-compatibility-plan.md)  
**Date:** June 2026

---

## Priority

Low. Object restore is relevant when cloud tiering (lifecycle transitions to a GLACIER-equivalent storage class) is in use. The misc tests cover minor error cases and Ceph-specific extensions that don't fit other RFCs.

## Design

`RestoreObject` is meaningful only when cloud storage classes are configured (lifecycle transitions via RFC 2.20). Without cloud tiering, `RestoreObject` on a standard-storage object returns `400 ObjectAlreadyInActiveTierError`. When cloud tiering is configured, restore initiates a copy from the remote tier back to local storage.

## Changes Required

**Object Restore** (`POST /s3/{bucket}/{key}?restore`):
- Parse XML body: `<RestoreRequest><Days>N</Days><GlacierJobParameters><Tier>Standard</Tier></GlacierJobParameters></RestoreRequest>`
- If object is not tiered: return `400 ObjectAlreadyInActiveTierError`
- If object is tiered: initiate restore job; return `202 Accepted`; on subsequent HEAD/GET, return `x-amz-restore: ongoing-request="true"` until done, then `x-amz-restore: ongoing-request="false", expiry-date="..."` for a temporary restore
- Permanent restore (non-expiring): `test_restore_object_permanent` — restore with no expiry
- Non-current version restore: `test_restore_noncur_obj` — restore a noncurrent version specifically
- ListObjects restore status: `?fetch-owner` includes restore status in listing; `test_list_objects_restore_status`, `test_list_object_versions_restore_status`

**Torrent (Ceph extension):**
- `GET /s3/{bucket}/{key}?torrent` — return a `.torrent` file for the object (Ceph-specific BitTorrent distribution feature)
- `test_get_object_torrent`: verify response is a valid torrent file

**Misc error cases:**
- `test_object_read_unreadable`: GET with a key containing invalid non-UTF-8 bytes returns `400 InvalidArgument` (covered in RFC 2.1 implementation; listed here for completeness)

## Ceph Tests Targeted

`test_restore_object_temporary`, `test_restore_object_permanent`, `test_restore_noncur_obj`, `test_list_objects_restore_status`, `test_list_object_versions_restore_status`, `test_get_object_torrent`
