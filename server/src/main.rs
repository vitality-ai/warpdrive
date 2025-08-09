use actix_web::{App, HttpServer};
use log::info;

/* storage.rs contains functionality for:
 * - Writing files to storage
 * - Managing file offsets and sizes
 * - Retrieving files from storage
 * - Deleting files from storage
 */ 
mod storage;

// Metadata storage abstraction layer
mod metadata;

// Configuration for metadata storage
mod config;

// SQLite implementation of metadata storage
mod sqlite_store;

// Mock implementation for testing
mod mock_store;

// Legacy database module for backward compatibility
mod database;

// Integration tests
#[cfg(test)]
mod integration_tests;

// Handles serialization and deserialization of file offsets and sizes into binary format for SQL blob storage
mod util;

mod api;
use crate::api::{put,get,append,delete,update_key,update};

mod service;
use log4rs;

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    // Initialize logging, server setup, etc.
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