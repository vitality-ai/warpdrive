// handlers/ — S3 request handlers split by concern.
pub(super) mod common;
pub(super) mod checksum;
pub(super) mod cors;
pub(super) mod tagging;
pub(super) mod versioning;
pub(super) mod acl;
pub(super) mod bucket;
pub(super) mod listing;
pub(super) mod object;
pub(super) mod copy;
pub(super) mod multipart;

pub use bucket::{s3_list_buckets_handler, s3_create_bucket_handler, s3_delete_bucket_handler, s3_head_bucket_handler};
pub use object::{s3_put_object_handler, s3_get_object_handler, s3_head_object_handler, s3_delete_object_handler};
pub use listing::{s3_list_objects_handler, s3_delete_objects_handler};
pub use copy::s3_copy_object_handler;
pub use multipart::{s3_create_multipart_upload_handler, s3_upload_part_handler, s3_upload_part_copy_handler, s3_complete_multipart_upload_handler, s3_abort_multipart_upload_handler, s3_multipart_router};
pub use cors::s3_cors_not_configured_handler;
