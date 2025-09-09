use actix_web::{App, HttpServer};
use log::info;
use log4rs;

use warp_drive::api::{put, get, append, delete, update_key, update}; 
// ^ use the name from your Cargo.toml [package].name
// e.g., if Cargo.toml says name = "warp_drive"
 
#[actix_web::main]
async fn main() -> std::io::Result<()> {
    log4rs::init_file("server_log.yaml", Default::default()).unwrap();
    info!("Starting server on 127.0.0.1:8080");
    
    HttpServer::new(|| {
        App::new()
            .wrap(actix_web::middleware::Logger::default())
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
