//api.rs
use actix_web::{web, HttpRequest, HttpResponse,Error };
use log::info;

use crate::service::{get_service, put_service ,append_service , delete_service, update_key_service,update_service};

#[actix_web::post("/put/{key}")]
async fn put(
    key: web::Path<String>,
    payload: web::Payload,
    req: HttpRequest,
) -> Result<HttpResponse, Error> {
    info!("Uploading data with key: {}", key);
    put_service(key.into_inner(), payload, req).await
}

#[actix_web::get("/get/{key}")]
async fn get(
    key: web::Path<String>,
    req: HttpRequest,
) -> Result<HttpResponse, Error> {
    info!("checking key and retrieving : {}", key);
    get_service(key.into_inner(), req).await
}


#[actix_web::post("/append/{key}")]
async fn append(
    key: web::Path<String>,
    payload: web::Payload,
    req: HttpRequest,
) -> Result<HttpResponse, Error> {
    info!("appending data with key: {}", key);
    append_service(key.into_inner(), payload, req).await
}

#[actix_web::delete("/delete/{key}")]
async fn delete(
    key: web::Path<String>,
    req: HttpRequest,
) -> Result<HttpResponse, Error> {
    info!("deleting data with key: {}", key);
    delete_service(key.into_inner(), req).await
}


#[actix_web::put("/update_key/{old_key}/{new_key}")]
async fn update_key(
    path: web::Path<(String, String)>,
    req: HttpRequest,
) -> Result<HttpResponse, Error> {
    let (old_key, new_key) = path.into_inner();
    info!("updating old key with key: {}", new_key);
    update_key_service(old_key, new_key, req).await
}


#[actix_web::post("/update/{key}")]
async fn update(
    key: web::Path<String>,
    payload: web::Payload,
    req: HttpRequest,
) -> Result<HttpResponse, Error> {
    info!("Uploading data with key: {}", key);
    update_service(key.into_inner(), payload, req).await
}