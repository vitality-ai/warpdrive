//! Distributed object engine: membership, placement, erasure coding, and the
//! coordinator that ties them together. See docs/Distributed-Engine-Plan.md.

pub mod ec;
pub mod packing;
pub mod placement;
pub mod reclaim;
pub mod compactor;
pub mod gc;
pub mod membership;
pub mod meta_engine;
pub mod shard_meta_store;
pub mod timing_stats;
pub mod shard_storage;
pub mod peer_client;
pub mod grpc_peer_client;
pub mod tcp_peer_client;
pub mod bridge;
pub mod blob_io;
pub mod blob_store;
pub mod rows;
pub mod meta_batch;
pub mod location_store;
pub mod content_location_store;
pub mod metadata_store;
pub mod bucket_config;
pub mod coordinator;
pub mod packed;
pub mod pushdown;

pub mod shard_proto {
    tonic::include_proto!("warpdrive.cluster");
}

#[allow(dead_code, non_snake_case, unused_imports)]
pub mod shard_wire_generated;
