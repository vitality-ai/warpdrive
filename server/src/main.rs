use actix_web::{App, HttpServer, web, HttpResponse};
use log::info;
use log4rs;

use warp_drive::api::{put, get, append, delete, update_key, update};
use warp_drive::metrics;
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
    s3_delete_objects_handler,
    s3_multipart_router,
    s3_cors_not_configured_handler,
};
use warp_drive::service::deletion_worker::start_deletion_worker;
use warp_drive::warpd::warpd_slab_batch_get;

async fn admin_metrics_handler() -> HttpResponse {
    #[cfg(feature = "op-counters")]
    {
        let counts = metrics::capture();
        let cost = counts.estimated_cost_usd();
        let latencies = metrics::capture_latencies();
        return HttpResponse::Ok().json(serde_json::json!({
            "ops": counts,
            "estimated_cost_usd": cost,
            "latencies": latencies,
        }));
    }
    #[cfg(not(feature = "op-counters"))]
    HttpResponse::Ok().body("op-counters feature not enabled; rebuild with --features op-counters\n")
}

async fn admin_metrics_reset_handler() -> HttpResponse {
    #[cfg(feature = "op-counters")]
    metrics::reset();
    HttpResponse::NoContent().finish()
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let _ = dotenvy::dotenv();
    log4rs::init_file("server_log.yaml", Default::default()).unwrap();
    info!("Starting HTTP server on 0.0.0.0:9710 (S3 under /s3/...)");

    let _deletion_worker_handle = start_deletion_worker();
    info!("Deletion worker started in background");

    HttpServer::new(|| {
        App::new()
            .wrap(actix_web::middleware::Logger::default())
            .app_data(web::PayloadConfig::default().limit(5 * 1024 * 1024 * 1024))
            // Admin endpoints
            .route("/_admin/metrics",       web::get().to(admin_metrics_handler))
            .route("/_admin/metrics/reset", web::post().to(admin_metrics_reset_handler))
            // WarpDrive-native: batch slab GET (1 round trip for all k delta layers)
            .route("/_warpd/slab/{bucket}", web::get().to(warpd_slab_batch_get))
            // S3-compatible API — prefixed form (/s3/...)
            .route("/s3",               web::get().to(s3_list_buckets_handler))
            .route("/s3/",              web::get().to(s3_list_buckets_handler))
            .route("/s3/{bucket}",      web::put().to(s3_create_bucket_handler))
            .route("/s3/{bucket}",      web::delete().to(s3_delete_bucket_handler))
            .route("/s3/{bucket}",      web::head().to(s3_head_bucket_handler))
            .route("/s3/{bucket}",      web::get().to(s3_list_objects_handler))
            .route("/s3/{bucket}",      web::post().to(s3_delete_objects_handler))
            .route("/s3/{bucket}/{key:.*}", web::put().to(s3_put_object_handler))
            .route("/s3/{bucket}/{key:.*}", web::get().to(s3_get_object_handler))
            .route("/s3/{bucket}/{key:.*}", web::delete().to(s3_delete_object_handler))
            .route("/s3/{bucket}/{key:.*}", web::head().to(s3_head_object_handler))
            .route("/s3/{bucket}/{key:.*}", web::post().to(s3_multipart_router))
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
            .route("/{bucket}",          web::get().to(s3_list_objects_handler))
            .route("/{bucket}",          web::post().to(s3_delete_objects_handler))
            // Trailing-slash bucket routes — aws-sdk-rust sends GET /bucket/?list-type=2
            .route("/{bucket}/",         web::put().to(s3_create_bucket_handler))
            .route("/{bucket}/",         web::delete().to(s3_delete_bucket_handler))
            .route("/{bucket}/",         web::head().to(s3_head_bucket_handler))
            .route("/{bucket}/",         web::get().to(s3_list_objects_handler))
            .route("/{bucket}/",         web::post().to(s3_delete_objects_handler))
            .route("/{bucket}/{key:.*}", web::put().to(s3_put_object_handler))
            .route("/{bucket}/{key:.*}", web::get().to(s3_get_object_handler))
            .route("/{bucket}/{key:.*}", web::delete().to(s3_delete_object_handler))
            .route("/{bucket}/{key:.*}", web::head().to(s3_head_object_handler))
            .route("/{bucket}/{key:.*}", web::post().to(s3_multipart_router))
            .route("/{bucket}",          web::method(actix_web::http::Method::OPTIONS).to(s3_cors_not_configured_handler))
            .route("/{bucket}/",         web::method(actix_web::http::Method::OPTIONS).to(s3_cors_not_configured_handler))
            .route("/{bucket}/{key:.*}", web::method(actix_web::http::Method::OPTIONS).to(s3_cors_not_configured_handler))
    })
    // Nagle's algorithm is enabled by default on every accepted connection
    // (actix-http's tcp_nodelay defaults to None/off). For large response
    // bodies written in bounded internal buffer flushes, Nagle batching
    // combined with the client's delayed-ACK timer can stall each flush
    // boundary, capping single-connection throughput far below the raw
    // network's capacity regardless of how the response body is built.
    .tcp_nodelay(true)
    .bind(("0.0.0.0", 9710))?
    .run()
    .await
}
