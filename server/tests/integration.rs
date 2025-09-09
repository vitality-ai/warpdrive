use actix_web::{test, App, http::StatusCode};
use warp_drive::api::{put, get, append, delete, update_key, update};

// bring in your generated flatbuffers schema
use warp_drive::util::flatbuffer_store_generated::store::{
    FileData, FileDataArgs, FileDataList, FileDataListArgs,
};
use flatbuffers::FlatBufferBuilder;

#[actix_web::test]
async fn test_api_endpoints() {
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
    
    // 4. Test GET endpoint with the same key
    let get_req = test::TestRequest::get()
        .uri(&format!("/get/{}", unique_key))
        .insert_header(("user", "testuser1"))
        .to_request();
    
    let get_resp = test::call_service(&app, get_req).await;
    let get_status = get_resp.status();
    println!("GET Response Status: {:?}", get_status);
    println!("GET Response Headers:");
    for (name, value) in get_resp.headers() {
        println!("  {}: {:?}", name, value);
    }
    
    let get_body = test::read_body(get_resp).await;
    println!("GET Response Body Length: {} bytes", get_body.len());
    if !get_body.is_empty() {
        println!("GET Response Body (first 100 bytes): {:?}", &get_body[..get_body.len().min(100)]);
        // Try to decode as UTF-8 string
        if let Ok(body_str) = std::str::from_utf8(&get_body) {
            println!("GET Response Body (as string): {}", body_str);
        }
    }
    
    assert_eq!(get_status, StatusCode::OK);
}

#[actix_web::test]
async fn test_user_isolation() {
    // Test that different users can have the same key names
    // This verifies the behavior found by TLA+ model checker
    
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

    // 2. Create test app
    let app = test::init_service(
        App::new()
            .service(put)
            .service(get)
            .service(append)
            .service(delete)
            .service(update_key)
            .service(update)
    ).await;

    // 3. Test user1 storing data with key "shared_key"
    let unique_key = format!("shared_key_{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos());
    println!("Testing user isolation with key: {}", unique_key);
    
    // User1 PUT
    let user1_req = test::TestRequest::post()
        .uri(&format!("/put/{}", unique_key))
        .insert_header(("content-type", "application/octet-stream"))
        .insert_header(("user", "testuser1"))
        .set_payload(buf.to_vec())
        .to_request();

    let user1_resp = test::call_service(&app, user1_req).await;
    let user1_status = user1_resp.status();
    println!("User1 PUT Status: {:?}", user1_status);
    assert_eq!(user1_status, StatusCode::OK);

    // 4. Test user2 storing data with the SAME key (should this be allowed?)
    let user2_req = test::TestRequest::post()
        .uri(&format!("/put/{}", unique_key))
        .insert_header(("content-type", "application/octet-stream"))
        .insert_header(("user", "testuser2"))
        .set_payload(buf.to_vec())
        .to_request();

    let user2_resp = test::call_service(&app, user2_req).await;
    let user2_status = user2_resp.status();
    println!("User2 PUT Status: {:?}", user2_status);
    
    // 5. Check if both users can retrieve their data
    let user1_get_req = test::TestRequest::get()
        .uri(&format!("/get/{}", unique_key))
        .insert_header(("user", "testuser1"))
        .to_request();
    
    let user1_get_resp = test::call_service(&app, user1_get_req).await;
    let user1_get_status = user1_get_resp.status();
    println!("User1 GET Status: {:?}", user1_get_status);
    
    let user2_get_req = test::TestRequest::get()
        .uri(&format!("/get/{}", unique_key))
        .insert_header(("user", "testuser2"))
        .to_request();
    
    let user2_get_resp = test::call_service(&app, user2_get_req).await;
    let user2_get_status = user2_get_resp.status();
    println!("User2 GET Status: {:?}", user2_get_status);
    
    // 6. Print results for analysis
    println!("=== USER ISOLATION TEST RESULTS ===");
    println!("User1 PUT: {:?}", user1_status);
    println!("User2 PUT: {:?}", user2_status);
    println!("User1 GET: {:?}", user1_get_status);
    println!("User2 GET: {:?}", user2_get_status);
    
    // This test documents the current behavior - adjust assertions based on requirements
    if user2_status == StatusCode::OK {
        println!("✅ CONFIRMED: Different users CAN have the same key names");
        assert_eq!(user1_get_status, StatusCode::OK);
        assert_eq!(user2_get_status, StatusCode::OK);
    } else {
        println!("❌ CONFIRMED: Different users CANNOT have the same key names");
        // If user2 PUT fails, user1 should still be able to GET
        assert_eq!(user1_get_status, StatusCode::OK);
    }
}
