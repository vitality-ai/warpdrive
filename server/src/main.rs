use actix_web::{App, Error, HttpRequest, HttpResponse, HttpServer, web};
use futures::StreamExt;
use log::info;
use log4rs;
use std::collections::HashMap;
use std::sync::Arc;

use warp_drive::api::{put, get, append, delete, update_key, update};
use warp_drive::s3::handlers::{
    s3_put_object_handler,
    s3_get_object_handler,
    s3_delete_object_handler,
    s3_head_object_handler,
    s3_head_bucket_handler,
    s3_list_objects_handler,
    s3_list_buckets_handler,
    s3_create_bucket_handler,
    s3_delete_bucket_handler,
    s3_multipart_router,
    s3_cors_not_configured_handler,
    s3_post_bucket_handler,
};
use warp_drive::service::deletion_worker::start_deletion_worker;
use warp_drive::cluster::coordinator::{
    cluster_put_object, cluster_get_object, cluster_delete_object, cluster_put_retention,
    cluster_join, cluster_internal_put_location, cluster_internal_delete_location,
    cluster_internal_put_content_location, cluster_internal_delete_content_location, cluster_content_record,
    cluster_put_bucket_config, cluster_internal_put_bucket_config,
    cluster_timing_stats, cluster_shard_server_timing, cluster_grpc_client_timing, ClusterState,
};
use warp_drive::cluster::pushdown::{cluster_pushdown_query, cluster_internal_pushdown_filter, ColumnCodec, ZlibF64Codec};
use warp_drive::cluster::s3_surface::{
    cluster_s3_delete_object_authenticated, cluster_s3_delete_objects_authenticated,
    cluster_s3_get_object_authenticated,
    cluster_s3_head_object, cluster_s3_head_object_authenticated,
    cluster_s3_list_objects, cluster_s3_multipart_post_authenticated, cluster_s3_put_object_authenticated,
};
use warp_drive::cluster::grpc_peer_client::{make_server as make_grpc_shard_server, GrpcPeerClient, GRPC_PORT_OFFSET};
use warp_drive::cluster::ec::{ErasureCoder, ReedSolomonCoder};
use warp_drive::cluster::location_store::BitcaskLocationStore;
use warp_drive::cluster::membership::Membership;
use warp_drive::cluster::peer_client::{HttpPeerClient, PeerClient};
use warp_drive::cluster::placement::ComputedPlacement;
use warp_drive::cluster::tcp_peer_client::{serve as serve_tcp_shard, TcpPeerClient, TCP_PORT_OFFSET};
use warp_drive::cluster::bucket_config::{BitcaskBucketConfigStore, BucketConfigStore};
use warp_drive::cluster::packing::StripePacker;

/// RS(k, m) for the cluster layer, `k` = data shards, `m` = parity shards
/// (`k+m` total). Dev-loop default is `k=3, m=2` (small local cluster, fast
/// iteration). To match Fusion's own default erasure code for a real
/// reproduction, set `WARPDRIVE_RS_K=6 WARPDRIVE_RS_M=3` — **not** 9 and 6.
/// Fusion's paper writes this as "RS(9,6)" in `(n, k)` notation: n=9 total
/// blocks, k=6 *data* blocks, parity = n-k = 3 (ASPLOS'25 Fig. 2: "A (9, 6)
/// erasure code... 6 data blocks and 3 parity blocks"). That's the opposite
/// of this project's own `(k, m)` = (data, parity) convention, so reading
/// their "9" as our `k` is the wrong translation — caught after an initial
/// run used k=9,m=6 (15 total nodes) by mistake. Per-bucket selection is
/// deferred (see docs/Distributed-Engine-Plan.md).
///
/// `m` must be `>= 1` -- the cluster engine is always erasure-coded. A
/// single, unreplicated node isn't a cluster config at all; use
/// single-node mode (`/s3/...`, SQLite metadata, no EC) for that instead
/// of asking the distributed engine to run with zero redundancy.
fn ec_params_from_env() -> (usize, usize) {
    let k = std::env::var("WARPDRIVE_RS_K").ok().and_then(|v| v.parse().ok()).unwrap_or(3);
    let m = std::env::var("WARPDRIVE_RS_M").ok().and_then(|v| v.parse().ok()).unwrap_or(2);
    (k, m)
}

/// Transport decision point (docs/Distributed-Engine-Plan.md): the
/// head-to-head benchmark (`transport_bench`) picked gRPC as the default —
/// ~35% lower median latency at 4KB, ~28% at 256KB, roughly tied at 2MB
/// where bulk transfer dominates over protocol overhead. HTTP and the newer
/// raw-TCP+FlatBuffers transport stay in the tree behind the same trait;
/// set `WARPDRIVE_PEER_TRANSPORT=http` or `=tcp` to use either instead
/// (e.g. to re-run the comparison on different hardware).
fn peer_client_from_env() -> Arc<dyn PeerClient> {
    match std::env::var("WARPDRIVE_PEER_TRANSPORT").as_deref() {
        Ok("http") => Arc::new(HttpPeerClient::new()),
        Ok("tcp") => Arc::new(TcpPeerClient::new()),
        _ => Arc::new(GrpcPeerClient::new()),
    }
}

/// Which engine should answer `/s3/...` (and root `/{bucket}/...`) for
/// this request. Three states, not two: a node can be configured for a
/// cluster (`WARPDRIVE_PEERS` or `WARPDRIVE_JOIN_VIA` given at startup --
/// `ClusterState::configured_for_cluster`) and still have no peers yet,
/// either because a dynamic join (`WARPDRIVE_JOIN_VIA`/`WARPDRIVE_SELF_ADDR`)
/// hasn't completed, or because it failed outright and never will.
/// Silently falling back to single-node storage in that window would let
/// a client write successfully, then lose that write the moment the join
/// does complete and routing switches to the cluster -- so a
/// configured-but-not-ready node must reject the request instead, not
/// quietly serve it from the wrong engine.
enum Engine {
    SingleNode,
    Distributed,
    NotReady,
}

fn pick_engine(state: &ClusterState) -> Engine {
    if !state.membership.peers().is_empty() {
        Engine::Distributed
    } else if state.configured_for_cluster {
        Engine::NotReady
    } else {
        Engine::SingleNode
    }
}

fn not_ready_error() -> Error {
    actix_web::error::ErrorServiceUnavailable(
        "this node is configured for the distributed cluster but hasn't finished joining yet \
         (or the join failed) -- refusing to silently serve this request from single-node storage",
    )
}

/// Buffers a streamed body into `web::Bytes`, the shape the cluster
/// handlers need (erasure coding has to see the whole object anyway).
/// Pre-sized from Content-Length when the client sends one -- actix's own
/// `web::Bytes` extractor (what the unchanged `/cluster/s3/...` routes
/// use) does the same, and growing a `Vec::new()` from zero one chunk at
/// a time instead measurably cost a few percent of PUT throughput in a
/// side-by-side benchmark against that route.
async fn buffer_payload(req: &HttpRequest, mut payload: web::Payload) -> Result<web::Bytes, Error> {
    let hint = req
        .headers()
        .get(actix_web::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = Vec::with_capacity(hint);
    while let Some(chunk) = payload.next().await {
        body.extend_from_slice(&chunk.map_err(actix_web::error::ErrorInternalServerError)?);
    }
    Ok(web::Bytes::from(body))
}

/// PutObject dispatch: single-node streams the body (`web::Payload`) to
/// avoid holding a large object fully in memory; the cluster path needs
/// the whole object in memory anyway to erasure-code it, so in
/// distributed mode this buffers the stream once up front instead of
/// duplicating single-node's own streaming write path for no benefit.
async fn s3_put_object_dispatch(
    path: web::Path<(String, String)>,
    payload: web::Payload,
    req: HttpRequest,
    cluster_state: web::Data<ClusterState>,
) -> Result<HttpResponse, Error> {
    match pick_engine(&cluster_state) {
        Engine::Distributed => {
            let body = buffer_payload(&req, payload).await?;
            cluster_s3_put_object_authenticated(path, body, req, cluster_state).await
        }
        Engine::NotReady => Err(not_ready_error()),
        Engine::SingleNode => s3_put_object_handler(path, payload, req).await,
    }
}

async fn s3_get_object_dispatch(
    path: web::Path<(String, String)>,
    req: HttpRequest,
    cluster_state: web::Data<ClusterState>,
) -> Result<HttpResponse, Error> {
    match pick_engine(&cluster_state) {
        Engine::Distributed => cluster_s3_get_object_authenticated(path, req, cluster_state).await,
        Engine::NotReady => Err(not_ready_error()),
        Engine::SingleNode => s3_get_object_handler(path, req).await,
    }
}

async fn s3_delete_object_dispatch(
    path: web::Path<(String, String)>,
    req: HttpRequest,
    cluster_state: web::Data<ClusterState>,
) -> Result<HttpResponse, Error> {
    match pick_engine(&cluster_state) {
        Engine::Distributed => cluster_s3_delete_object_authenticated(path, req, cluster_state).await,
        Engine::NotReady => Err(not_ready_error()),
        Engine::SingleNode => s3_delete_object_handler(path, req).await,
    }
}

async fn s3_head_object_dispatch(
    path: web::Path<(String, String)>,
    req: HttpRequest,
    cluster_state: web::Data<ClusterState>,
) -> Result<HttpResponse, Error> {
    match pick_engine(&cluster_state) {
        Engine::Distributed => cluster_s3_head_object_authenticated(path, req, cluster_state).await,
        Engine::NotReady => Err(not_ready_error()),
        Engine::SingleNode => s3_head_object_handler(path, req).await,
    }
}

/// ListObjectsV2 dispatch (bucket-level GET). Bucket lifecycle itself
/// (create/delete/head-bucket) stays single-node-only in both modes --
/// the cluster engine has no bucket registry to hang that on (a
/// deliberate, already-documented scope limit, not new here), so there's
/// no cluster equivalent to dispatch to. Bucket-level POST is handled
/// separately below (`s3_post_bucket_dispatch`): its `?delete` branch is
/// an object-data-plane operation in disguise and does have a cluster
/// equivalent, unlike the rest of bucket lifecycle.
async fn s3_list_objects_dispatch(
    path: web::Path<String>,
    query: web::Query<HashMap<String, String>>,
    req: HttpRequest,
    cluster_state: web::Data<ClusterState>,
) -> Result<HttpResponse, Error> {
    match pick_engine(&cluster_state) {
        Engine::Distributed => cluster_s3_list_objects(path, query, req, cluster_state).await,
        Engine::NotReady => Err(not_ready_error()),
        Engine::SingleNode => s3_list_objects_handler(path, query, req).await,
    }
}

async fn s3_multipart_dispatch(
    path: web::Path<(String, String)>,
    query: web::Query<HashMap<String, String>>,
    payload: web::Payload,
    req: HttpRequest,
    cluster_state: web::Data<ClusterState>,
) -> Result<HttpResponse, Error> {
    match pick_engine(&cluster_state) {
        Engine::Distributed => {
            let body = buffer_payload(&req, payload).await?;
            cluster_s3_multipart_post_authenticated(path, body, req, cluster_state).await
        }
        Engine::NotReady => Err(not_ready_error()),
        Engine::SingleNode => s3_multipart_router(path, query, payload, req).await,
    }
}

/// Bucket-level POST dispatch. Two very different operations share this
/// one verb: `?delete` (DeleteObjects, bulk delete -- object-data-plane,
/// dispatched the same way as every other object operation above) and
/// plain POST (RFC 2.14 HTML form upload, which has no cluster
/// equivalent and never claimed one -- stays single-node-only in both
/// modes, same as bucket lifecycle).
async fn s3_post_bucket_dispatch(
    path: web::Path<String>,
    query: web::Query<HashMap<String, String>>,
    payload: web::Payload,
    req: HttpRequest,
    cluster_state: web::Data<ClusterState>,
) -> Result<HttpResponse, Error> {
    if query.contains_key("delete") {
        match pick_engine(&cluster_state) {
            Engine::Distributed => {
                let body = buffer_payload(&req, payload).await?;
                return cluster_s3_delete_objects_authenticated(path, body, req, cluster_state).await;
            }
            Engine::NotReady => return Err(not_ready_error()),
            Engine::SingleNode => {}
        }
    }
    // Plain POST (form upload) has no cluster equivalent and is never
    // gated on cluster readiness -- it was never going to touch cluster
    // storage either way.
    s3_post_bucket_handler(path, query, payload, req).await
}

fn build_cluster_state() -> web::Data<ClusterState> {
    let (k, m) = ec_params_from_env();
    let location_log = std::env::var("WARPDRIVE_LOCATION_LOG")
        .unwrap_or_else(|_| "cluster_location.log".to_string());

    let state = ClusterState {
        membership: Arc::new(Membership::from_env()),
        placement: Arc::new(ComputedPlacement::new()),
        ec: Arc::new(ReedSolomonCoder::new(k, m).expect("invalid RS(k,m) configuration")) as Arc<dyn ErasureCoder>,
        location_store: Arc::new(
            BitcaskLocationStore::open(&location_log).expect("failed to open location store log"),
        ),
        peer_client: peer_client_from_env(),
        // Used for pin/tombstone replication to *every* peer and for
        // pushdown forwarding (#162): a default reqwest::Client has no
        // timeout at all, so one unreachable or black-holed peer could
        // stall every PUT/DELETE/pushdown-query in the cluster
        // indefinitely. 10s is generous for a same-datacenter metadata
        // call; a hung peer now fails that one request instead of
        // hanging the coordinator forever.
        location_http: reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .connect_timeout(std::time::Duration::from_secs(3))
            .build()
            .expect("failed to build location_http client"),
        timing: Arc::new(warp_drive::cluster::timing_stats::TimingStats::default()),
        // Registry, not a single packer: a bucket's config (bucket_config.rs)
        // names which entry to use. "fac" is the one shipped implementation;
        // a user-defined packer becomes a third pluggable policy by
        // implementing StripePacker and registering it here under its own
        // name, not by modifying coordinator.rs.
        packers: {
            let mut m: std::collections::HashMap<String, Arc<dyn StripePacker>> = std::collections::HashMap::new();
            m.insert("fac".to_string(), Arc::new(warp_drive::cluster::packing::FacPacker));
            m.insert("ivf_centroid".to_string(), Arc::new(warp_drive::cluster::packing::IvfCentroidPacker));
            m
        },
        content_location_store: Arc::new(
            warp_drive::cluster::content_location_store::BitcaskContentLocationStore::open(
                std::env::var("WARPDRIVE_CONTENT_LOCATION_LOG")
                    .unwrap_or_else(|_| "cluster_content_location.log".to_string()),
            )
            .expect("failed to open content location store log"),
        ),
        bucket_config_store: Arc::new(
            BitcaskBucketConfigStore::open(
                std::env::var("WARPDRIVE_BUCKET_CONFIG_LOG")
                    .unwrap_or_else(|_| "cluster_bucket_config.log".to_string()),
            )
            .expect("failed to open bucket config store log"),
        ) as Arc<dyn BucketConfigStore>,
        column_codecs: {
            let mut m: std::collections::HashMap<String, Arc<dyn ColumnCodec>> = std::collections::HashMap::new();
            m.insert("zlib_f64".to_string(), Arc::new(ZlibF64Codec));
            m
        },
        configured_for_cluster: std::env::var("WARPDRIVE_PEERS").map(|v| !v.trim().is_empty()).unwrap_or(false)
            || std::env::var("WARPDRIVE_JOIN_VIA").is_ok(),
    };
    web::Data::new(state)
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let _ = dotenvy::dotenv();
    log4rs::init_file("server_log.yaml", Default::default()).unwrap();
    let port: u16 = std::env::var("WARPDRIVE_PORT").ok().and_then(|v| v.parse().ok()).unwrap_or(9710);
    // Informational only -- the real per-request decision (`distributed_mode`,
    // below) is made from live membership, not this startup snapshot. A
    // join-only node (WARPDRIVE_JOIN_VIA set, no WARPDRIVE_PEERS) starts
    // with an empty peer list and only becomes a cluster member once its
    // spawned join task (below) actually completes.
    let configured_for_cluster = std::env::var("WARPDRIVE_PEERS").map(|v| !v.trim().is_empty()).unwrap_or(false)
        || std::env::var("WARPDRIVE_JOIN_VIA").is_ok();
    info!(
        "Starting HTTP server on 0.0.0.0:{port} (S3 under /s3/... and /, configured for cluster: {configured_for_cluster})"
    );

    let _deletion_worker_handle = start_deletion_worker();
    info!("Deletion worker started in background");

    let cluster_state = build_cluster_state();

    // Join an existing cluster at startup, if configured. Both env vars
    // are required together: WARPDRIVE_SELF_ADDR is how other nodes should
    // reach this one (can't be inferred reliably from a 0.0.0.0 bind).
    if let (Ok(join_via), Ok(self_addr)) = (
        std::env::var("WARPDRIVE_JOIN_VIA"),
        std::env::var("WARPDRIVE_SELF_ADDR"),
    ) {
        let membership = Arc::clone(&cluster_state.membership);
        tokio::spawn(async move {
            info!("Joining cluster via {join_via} as {self_addr}");
            warp_drive::cluster::membership::join_cluster_via(&join_via, &self_addr, &membership).await;
            info!("Join complete, peers now: {:?}", membership.peers());
        });
    }

    // gRPC shard server runs alongside actix-web, on port+1000 — the
    // other half of the transport decision point. Always runs regardless
    // of which PeerClient is selected for outbound calls, so either node
    // in a benchmark pair can be dialed over gRPC.
    let grpc_port = port + GRPC_PORT_OFFSET;
    tokio::spawn(async move {
        let addr = format!("0.0.0.0:{grpc_port}").parse().expect("invalid gRPC bind address");
        info!("Starting gRPC shard server on {addr}");
        if let Err(e) = tonic::transport::Server::builder()
            .add_service(make_grpc_shard_server())
            .serve(addr)
            .await
        {
            log::error!("gRPC shard server failed: {e}");
        }
    });

    // Raw-TCP+FlatBuffers shard server, on port+2000 — the third transport
    // candidate, also always running regardless of which PeerClient is
    // selected for outbound calls.
    let tcp_port = port + TCP_PORT_OFFSET;
    tokio::spawn(async move {
        let addr = format!("0.0.0.0:{tcp_port}").parse().expect("invalid TCP bind address");
        info!("Starting raw-TCP shard server on {addr}");
        if let Err(e) = serve_tcp_shard(addr).await {
            log::error!("TCP shard server failed: {e}");
        }
    });

    HttpServer::new(move || {
        App::new()
            .wrap(actix_web::middleware::Logger::default())
            .app_data(web::PayloadConfig::default().limit(5 * 1024 * 1024 * 1024))
            .app_data(cluster_state.clone())
            // Cluster API — distributed, erasure-coded PUT/GET/DELETE.
            // Specific/internal routes registered before the generic
            // {bucket}/{key:.*} catch-alls so they aren't shadowed.
            .route("/cluster/join", web::post().to(cluster_join))
            .route("/cluster/_internal/timing", web::get().to(cluster_timing_stats))
            .route("/cluster/_internal/shard_timing", web::get().to(cluster_shard_server_timing))
            .route("/cluster/_internal/grpc_client_timing", web::get().to(cluster_grpc_client_timing))
            .route("/cluster/_internal/location", web::post().to(cluster_internal_put_location))
            .route("/cluster/_internal/content_location", web::post().to(cluster_internal_put_content_location))
            .route("/cluster/_internal/location", web::delete().to(cluster_internal_delete_location))
            .route("/cluster/_internal/content_location", web::delete().to(cluster_internal_delete_content_location))
            .route("/cluster/_internal/content_record/{bucket}/{key:.*}", web::get().to(cluster_content_record))
            .route("/cluster/_internal/bucket_config", web::post().to(cluster_internal_put_bucket_config))
            .route("/cluster/_internal/pushdown_filter", web::post().to(cluster_internal_pushdown_filter))
            .route("/cluster/_admin/bucket_config/{bucket}", web::put().to(cluster_put_bucket_config))
            .route("/cluster/{bucket}/{key:.*}/retention", web::put().to(cluster_put_retention))
            .route("/cluster/{bucket}/{key:.*}/query", web::post().to(cluster_pushdown_query))
            // Minimal S3-protocol surface (s3_surface.rs) — a distinct "s3/" prefix
            // so it never collides with the generic {bucket}/{key:.*} routes
            // below. Phase 3: PUT/GET/DELETE/HEAD here now go through
            // authenticate_s3_request (same SigV4 verification single-node
            // uses) via dedicated *_authenticated handlers that delegate to
            // the *_inner functions for the actual EC/placement work -- a
            // deliberate, intended breaking change for this prefix
            // specifically (it was always documented as "no SigV4
            // verification... not a production S3 surface"; this is the
            // upgrade). The bare /cluster/{bucket}/{key:.*} routes below are
            // untouched and stay fully unauthenticated.
            .route("/cluster/s3/{bucket}", web::get().to(cluster_s3_list_objects))
            .route("/cluster/s3/{bucket}/{key:.*}", web::head().to(cluster_s3_head_object_authenticated))
            .route("/cluster/s3/{bucket}/{key:.*}", web::put().to(cluster_s3_put_object_authenticated))
            .route("/cluster/s3/{bucket}/{key:.*}", web::get().to(cluster_s3_get_object_authenticated))
            .route("/cluster/s3/{bucket}/{key:.*}", web::delete().to(cluster_s3_delete_object_authenticated))
            .route("/cluster/s3/{bucket}/{key:.*}", web::post().to(cluster_s3_multipart_post_authenticated))
            .route("/cluster/{bucket}/{key:.*}", web::put().to(cluster_put_object))
            .route("/cluster/{bucket}/{key:.*}", web::get().to(cluster_get_object))
            .route("/cluster/{bucket}/{key:.*}", web::delete().to(cluster_delete_object))
            .route("/cluster/{bucket}/{key:.*}", web::head().to(cluster_s3_head_object))
            // S3-compatible API — prefixed form (/s3/...)
            .route("/s3",               web::get().to(s3_list_buckets_handler))
            .route("/s3/",              web::get().to(s3_list_buckets_handler))
            .route("/s3/{bucket}",      web::put().to(s3_create_bucket_handler))
            .route("/s3/{bucket}",      web::delete().to(s3_delete_bucket_handler))
            .route("/s3/{bucket}",      web::head().to(s3_head_bucket_handler))
            .route("/s3/{bucket}",      web::get().to(s3_list_objects_dispatch))
            .route("/s3/{bucket}",      web::post().to(s3_post_bucket_dispatch))
            .route("/s3/{bucket}/{key:.*}", web::put().to(s3_put_object_dispatch))
            .route("/s3/{bucket}/{key:.*}", web::get().to(s3_get_object_dispatch))
            .route("/s3/{bucket}/{key:.*}", web::delete().to(s3_delete_object_dispatch))
            .route("/s3/{bucket}/{key:.*}", web::head().to(s3_head_object_dispatch))
            .route("/s3/{bucket}/{key:.*}", web::post().to(s3_multipart_dispatch))
            .route("/s3/{bucket}",          web::method(actix_web::http::Method::OPTIONS).to(s3_cors_not_configured_handler))
            .route("/s3/{bucket}/{key:.*}", web::method(actix_web::http::Method::OPTIONS).to(s3_cors_not_configured_handler))
            // Original native API (registered before root S3 routes to take priority on conflicts)
            .service(put)
            .service(get)
            .service(append)
            .service(delete)
            .service(update_key)
            .service(update)
            // S3-compatible API — root form (/{bucket}/...) for standard boto3 / Ceph s3-tests
            .route("/",                  web::get().to(s3_list_buckets_handler))
            .route("/{bucket}",          web::put().to(s3_create_bucket_handler))
            .route("/{bucket}",          web::delete().to(s3_delete_bucket_handler))
            .route("/{bucket}",          web::head().to(s3_head_bucket_handler))
            .route("/{bucket}",          web::get().to(s3_list_objects_dispatch))
            .route("/{bucket}",          web::post().to(s3_post_bucket_dispatch))
            // Trailing-slash bucket routes — aws-sdk-rust sends GET /bucket/?list-type=2
            .route("/{bucket}/",         web::put().to(s3_create_bucket_handler))
            .route("/{bucket}/",         web::delete().to(s3_delete_bucket_handler))
            .route("/{bucket}/",         web::head().to(s3_head_bucket_handler))
            .route("/{bucket}/",         web::get().to(s3_list_objects_dispatch))
            .route("/{bucket}/",         web::post().to(s3_post_bucket_dispatch))
            .route("/{bucket}/{key:.*}", web::put().to(s3_put_object_dispatch))
            .route("/{bucket}/{key:.*}", web::get().to(s3_get_object_dispatch))
            .route("/{bucket}/{key:.*}", web::delete().to(s3_delete_object_dispatch))
            .route("/{bucket}/{key:.*}", web::head().to(s3_head_object_dispatch))
            .route("/{bucket}/{key:.*}", web::post().to(s3_multipart_dispatch))
            .route("/{bucket}",          web::method(actix_web::http::Method::OPTIONS).to(s3_cors_not_configured_handler))
            .route("/{bucket}/",         web::method(actix_web::http::Method::OPTIONS).to(s3_cors_not_configured_handler))
            .route("/{bucket}/{key:.*}", web::method(actix_web::http::Method::OPTIONS).to(s3_cors_not_configured_handler))
    })
    .bind(("0.0.0.0", port))?
    .run()
    .await
}
