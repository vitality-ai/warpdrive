use actix_web::{App, HttpServer, web};
use log::info;
use log4rs;

use warp_drive::api::{put, get, append, delete, update_key, update}; 
use warp_drive::s3::handlers::{
    s3_put_object_handler,
    s3_get_object_handler,
    s3_delete_object_handler,
    s3_head_object_handler,
    s3_list_objects_handler,
    s3_multipart_router
};
use warp_drive::service::deletion_worker::start_deletion_worker;
// ^ use the name from your Cargo.toml [package].name
// e.g., if Cargo.toml says name = "warp_drive"
 
#[actix_web::main]
async fn main() -> std::io::Result<()> {
    log4rs::init_file("server_log.yaml", Default::default()).unwrap();
    info!("Starting server on 127.0.0.1:8080");
    
    // Start the deletion worker as a background task (non-blocking)
    let _deletion_worker_handle = start_deletion_worker();
    info!("Deletion worker started in background");
    
        HttpServer::new(|| {
            App::new()
                .wrap(actix_web::middleware::Logger::default())
                // Configure payload size limits for large files (up to 5GB - S3 standard)
                .app_data(web::PayloadConfig::default().limit(5 * 1024 * 1024 * 1024))
                // S3-compatible API endpoints with /s3/ prefix to avoid conflicts
                .route("/s3/{bucket}/{key:.*}", web::put().to(s3_put_object_handler))
                .route("/s3/{bucket}/{key:.*}", web::get().to(s3_get_object_handler))
                .route("/s3/{bucket}/{key:.*}", web::delete().to(s3_delete_object_handler))
                .route("/s3/{bucket}/{key:.*}", web::head().to(s3_head_object_handler))
                .route("/s3/{bucket}", web::get().to(s3_list_objects_handler))
                // Multipart upload endpoints - these need to be handled by a router
                .route("/s3/{bucket}/{key:.*}", web::post().to(s3_multipart_router))
                // Original API endpoints (must come after S3 routes)
                .service(put)
                .service(get)
                .service(append)
                .service(delete)
                .service(update_key)
                .service(update)
        })
    .bind(("0.0.0.0", 9710))?
    .run()
    .await
}
