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
use warp_drive::storage::deletion_worker::start_deletion_worker;
use warp_drive::app_state::AppState;
// ^ use the name from your Cargo.toml [package].name
// e.g., if Cargo.toml says name = "warp_drive"
 
#[actix_web::main]
async fn main() -> std::io::Result<()> {
    // Load configuration first
    let config = warp_drive::config::AppConfig::load().expect("Failed to load configuration");
    
    // Initialize logging with configured log file
    log4rs::init_file(&config.logging.config_file, Default::default()).unwrap();
    info!("Starting server on {}:{}", config.server.host, config.server.port);
    
    // Start the deletion worker as a background task (non-blocking)
    let _deletion_worker_handle = start_deletion_worker();
    info!("Deletion worker started in background");
    
    // Create application state
    let app_state = web::Data::new(AppState::from_config(config.clone()));
    info!("Application state initialized");
    
    // Extract server configuration for HttpServer
    let server_host = config.server.host.clone();
    let server_port = config.server.port;
    let server_workers = config.server.workers;
    let max_payload_size = config.server.max_payload_size;
    
        HttpServer::new(move || {
            App::new()
                .app_data(app_state.clone())
                .wrap(actix_web::middleware::Logger::default())
                // Configure payload size limits from configuration
                .app_data(web::PayloadConfig::default().limit(max_payload_size as usize))
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
    .workers(server_workers)
    .bind((server_host.as_str(), server_port))?
    .run()
    .await
}
