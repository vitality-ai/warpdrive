use actix_web::{test, App, http::StatusCode};
use warp_drive::api::{put, get, append, delete, update_key, update};

// bring in your generated flatbuffers schema
use warp_drive::util::flatbuffer_store_generated::store::{
    FileData, FileDataArgs, FileDataList, FileDataListArgs,
};
use flatbuffers::FlatBufferBuilder;

#[actix_web::test]
async fn test_put_flatbuffer() {
    // 1. Build a flatbuffer payload
    let mut builder = FlatBufferBuilder::new();

    let data_bytes = builder.create_vector(&[1u8, 2, 3, 4]);
    let file = FileData::create(&mut builder, &FileDataArgs {
        data: Some(data_bytes),
    });

    let files = builder.create_vector(&[file]);
    let file_list = FileDataList::create(&mut builder, &FileDataListArgs {
        files: Some(files),
    });

    builder.finish(file_list, None);
    let buf = builder.finished_data();

    // 2. Create test app with actix-web
    let app = test::init_service(
        App::new()
            .service(put)
            .service(get)
            .service(append)
            .service(delete)
            .service(update_key)
            .service(update)
    ).await;

    // 3. Send request with actix-web test client
    // Use a unique key to avoid "Key already exists" error
    let unique_key = format!("testkey_{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos());
    let uri = format!("/put/{}", unique_key);
    println!("Using key: {}", unique_key);
    
    let req = test::TestRequest::post()
        .uri(&uri)
        .insert_header(("content-type", "application/octet-stream"))
        .insert_header(("user", "testuser1"))
        .set_payload(buf.to_vec())
        .to_request();

    let resp = test::call_service(&app, req).await;
    
    // Print response details
    let status = resp.status();
    println!("Response Status: {:?}", status);
    println!("Response Headers:");
    println!("Just added to fake a change:");
    for (name, value) in resp.headers() {
        println!("  {}: {:?}", name, value);
    }
    
    // Get response body
    let body = test::read_body(resp).await;
    println!("Response Body Length: {} bytes", body.len());
    if !body.is_empty() {
        println!("Response Body (first 100 bytes): {:?}", &body[..body.len().min(100)]);
        // Try to decode as UTF-8 string
        if let Ok(body_str) = std::str::from_utf8(&body) {
            println!("Response Body (as string): {}", body_str);
        }
    }
    
    assert_eq!(status, StatusCode::OK);
}
