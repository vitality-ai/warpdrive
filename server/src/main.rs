use actix_web::{App, HttpServer, web};
use log::info;
use log4rs;

use warp_drive::api::{put, get, append, delete, update_key, update}; 
use warp_drive::s3::handlers::{
    s3_put_object_handler, 
    s3_get_object_handler, 
    s3_delete_object_handler,
    s3_head_object_handler,
    s3_list_objects_handler
};
// ^ use the name from your Cargo.toml [package].name
// e.g., if Cargo.toml says name = "warp_drive"
 
#[actix_web::main]
async fn main() -> std::io::Result<()> {
    log4rs::init_file("server_log.yaml", Default::default()).unwrap();
    info!("Starting server on 127.0.0.1:8080");
    
    HttpServer::new(|| {
        App::new()
            .wrap(actix_web::middleware::Logger::default())
            // Original API endpoints
            .service(put)
            .service(get)
            .service(append)
            .service(delete)
            .service(update_key)
            .service(update)
            // S3-compatible API endpoints
            .service(
                web::scope("/s3")
                    .route("/{bucket}/{key}", web::put().to(s3_put_object_handler))
                    .route("/{bucket}/{key}", web::get().to(s3_get_object_handler))
                    .route("/{bucket}/{key}", web::delete().to(s3_delete_object_handler))
                    .route("/{bucket}/{key}", web::head().to(s3_head_object_handler))
                    .route("/{bucket}", web::get().to(s3_list_objects_handler))
            )
    })
    .bind(("0.0.0.0", 9710))?
    .run()
    .await
}
