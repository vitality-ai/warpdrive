// S3 Middleware for request processing
use actix_web::{HttpRequest, HttpResponse, Error};
use log::info;

use crate::s3::auth::authenticate_s3_request;

/// Simple S3 request handler that processes requests without middleware complexity
pub async fn handle_s3_request(req: HttpRequest) -> Result<HttpResponse, Error> {
    info!("Handling S3 request: {}", req.path());
    
    // Authenticate the request
    let auth_result = authenticate_s3_request(&req)?;
    info!("S3 authentication successful for user: {}, bucket: {}", auth_result.user_id, auth_result.bucket);
    
    // Return success response
    Ok(HttpResponse::Ok().json(serde_json::json!({
        "status": "authenticated",
        "user_id": auth_result.user_id,
        "bucket": auth_result.bucket,
        "access_key": auth_result.access_key
    })))
}
