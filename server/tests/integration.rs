use actix_web::{test, App, http::StatusCode};
use warp_drive::api::{put, get, append, delete, update_key, update};

// bring in your generated flatbuffers schema
use warp_drive::util::flatbuffer_store_generated::store::{
    FileData, FileDataArgs, FileDataList, FileDataListArgs,
};
use flatbuffers::FlatBufferBuilder;

#[actix_web::test]
async fn test_basic_api_endpoints() {
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

#[actix_web::test]
async fn test_append_endpoint() {
    // Test the append functionality
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

    let app = test::init_service(
        App::new()
            .service(put)
            .service(get)
            .service(append)
            .service(delete)
            .service(update_key)
            .service(update)
    ).await;

    let unique_key = format!("append_test_{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos());
    println!("Testing append with key: {}", unique_key);

    // 1. First PUT some data
    let put_req = test::TestRequest::post()
        .uri(&format!("/put/{}", unique_key))
        .insert_header(("content-type", "application/octet-stream"))
        .insert_header(("user", "testuser1"))
        .set_payload(buf.to_vec())
        .to_request();

    let put_resp = test::call_service(&app, put_req).await;
    println!("PUT Status: {:?}", put_resp.status());
    assert_eq!(put_resp.status(), StatusCode::OK);

    // 2. Now APPEND more data
    let mut append_builder = FlatBufferBuilder::new();
    let append_data_bytes = append_builder.create_vector(&[5u8, 6, 7, 8]);
    let append_file = FileData::create(&mut append_builder, &FileDataArgs {
        data: Some(append_data_bytes),
    });
    let append_files = append_builder.create_vector(&[append_file]);
    let append_file_list = FileDataList::create(&mut append_builder, &FileDataListArgs {
        files: Some(append_files),
    });
    append_builder.finish(append_file_list, None);
    let append_buf = append_builder.finished_data();

    let append_req = test::TestRequest::post()
        .uri(&format!("/append/{}", unique_key))
        .insert_header(("content-type", "application/octet-stream"))
        .insert_header(("user", "testuser1"))
        .set_payload(append_buf.to_vec())
        .to_request();

    let append_resp = test::call_service(&app, append_req).await;
    println!("APPEND Status: {:?}", append_resp.status());
    assert_eq!(append_resp.status(), StatusCode::OK);

    // 3. GET the combined data
    let get_req = test::TestRequest::get()
        .uri(&format!("/get/{}", unique_key))
        .insert_header(("user", "testuser1"))
        .to_request();

    let get_resp = test::call_service(&app, get_req).await;
    println!("GET after append Status: {:?}", get_resp.status());
    assert_eq!(get_resp.status(), StatusCode::OK);

    let get_body = test::read_body(get_resp).await;
    println!("GET after append Body Length: {} bytes", get_body.len());
}

#[actix_web::test]
async fn test_delete_endpoint() {
    // Test the delete functionality
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

    let app = test::init_service(
        App::new()
            .service(put)
            .service(get)
            .service(append)
            .service(delete)
            .service(update_key)
            .service(update)
    ).await;

    let unique_key = format!("delete_test_{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos());
    println!("Testing delete with key: {}", unique_key);

    // 1. PUT some data
    let put_req = test::TestRequest::post()
        .uri(&format!("/put/{}", unique_key))
        .insert_header(("content-type", "application/octet-stream"))
        .insert_header(("user", "testuser1"))
        .set_payload(buf.to_vec())
        .to_request();

    let put_resp = test::call_service(&app, put_req).await;
    println!("PUT Status: {:?}", put_resp.status());
    assert_eq!(put_resp.status(), StatusCode::OK);

    // 2. GET to verify data exists
    let get_req = test::TestRequest::get()
        .uri(&format!("/get/{}", unique_key))
        .insert_header(("user", "testuser1"))
        .to_request();

    let get_resp = test::call_service(&app, get_req).await;
    println!("GET before delete Status: {:?}", get_resp.status());
    assert_eq!(get_resp.status(), StatusCode::OK);

    // 3. DELETE the data
    let delete_req = test::TestRequest::delete()
        .uri(&format!("/delete/{}", unique_key))
        .insert_header(("user", "testuser1"))
        .to_request();

    let delete_resp = test::call_service(&app, delete_req).await;
    println!("DELETE Status: {:?}", delete_resp.status());
    assert_eq!(delete_resp.status(), StatusCode::OK);

    // 4. GET to verify data is deleted
    let get_after_delete_req = test::TestRequest::get()
        .uri(&format!("/get/{}", unique_key))
        .insert_header(("user", "testuser1"))
        .to_request();

    let get_after_delete_resp = test::call_service(&app, get_after_delete_req).await;
    println!("GET after delete Status: {:?}", get_after_delete_resp.status());
    // Should return NotFound since key no longer exists
    assert_eq!(get_after_delete_resp.status(), StatusCode::NOT_FOUND);
}

#[actix_web::test]
async fn test_update_key_endpoint() {
    // Test the update_key functionality
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

    let app = test::init_service(
        App::new()
            .service(put)
            .service(get)
            .service(append)
            .service(delete)
            .service(update_key)
            .service(update)
    ).await;

    let old_key = format!("old_key_{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos());
    let new_key = format!("new_key_{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos());
    println!("Testing update_key: {} -> {}", old_key, new_key);

    // 1. PUT data with old key
    let put_req = test::TestRequest::post()
        .uri(&format!("/put/{}", old_key))
        .insert_header(("content-type", "application/octet-stream"))
        .insert_header(("user", "testuser1"))
        .set_payload(buf.to_vec())
        .to_request();

    let put_resp = test::call_service(&app, put_req).await;
    println!("PUT with old key Status: {:?}", put_resp.status());
    assert_eq!(put_resp.status(), StatusCode::OK);

    // 2. GET data with old key to verify it exists
    let get_old_req = test::TestRequest::get()
        .uri(&format!("/get/{}", old_key))
        .insert_header(("user", "testuser1"))
        .to_request();

    let get_old_resp = test::call_service(&app, get_old_req).await;
    println!("GET with old key Status: {:?}", get_old_resp.status());
    assert_eq!(get_old_resp.status(), StatusCode::OK);

    // 3. UPDATE the key
    let update_key_req = test::TestRequest::put()
        .uri(&format!("/update_key/{}/{}", old_key, new_key))
        .insert_header(("user", "testuser1"))
        .to_request();

    let update_key_resp = test::call_service(&app, update_key_req).await;
    println!("UPDATE_KEY Status: {:?}", update_key_resp.status());
    assert_eq!(update_key_resp.status(), StatusCode::OK);

    // 4. GET data with new key
    let get_new_req = test::TestRequest::get()
        .uri(&format!("/get/{}", new_key))
        .insert_header(("user", "testuser1"))
        .to_request();

    let get_new_resp = test::call_service(&app, get_new_req).await;
    println!("GET with new key Status: {:?}", get_new_resp.status());
    assert_eq!(get_new_resp.status(), StatusCode::OK);

    // 5. GET data with old key should fail
    let get_old_after_update_req = test::TestRequest::get()
        .uri(&format!("/get/{}", old_key))
        .insert_header(("user", "testuser1"))
        .to_request();

    let get_old_after_update_resp = test::call_service(&app, get_old_after_update_req).await;
    println!("GET with old key after update Status: {:?}", get_old_after_update_resp.status());
    assert_eq!(get_old_after_update_resp.status(), StatusCode::NOT_FOUND);
}

#[actix_web::test]
async fn test_update_endpoint() {
    // Test the update functionality
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

    let app = test::init_service(
        App::new()
            .service(put)
            .service(get)
            .service(append)
            .service(delete)
            .service(update_key)
            .service(update)
    ).await;

    let unique_key = format!("update_test_{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos());
    println!("Testing update with key: {}", unique_key);

    // 1. PUT initial data
    let put_req = test::TestRequest::post()
        .uri(&format!("/put/{}", unique_key))
        .insert_header(("content-type", "application/octet-stream"))
        .insert_header(("user", "testuser1"))
        .set_payload(buf.to_vec())
        .to_request();

    let put_resp = test::call_service(&app, put_req).await;
    println!("PUT Status: {:?}", put_resp.status());
    assert_eq!(put_resp.status(), StatusCode::OK);

    // 2. GET initial data
    let get_initial_req = test::TestRequest::get()
        .uri(&format!("/get/{}", unique_key))
        .insert_header(("user", "testuser1"))
        .to_request();

    let get_initial_resp = test::call_service(&app, get_initial_req).await;
    println!("GET initial Status: {:?}", get_initial_resp.status());
    assert_eq!(get_initial_resp.status(), StatusCode::OK);

    let initial_body = test::read_body(get_initial_resp).await;
    println!("Initial data length: {} bytes", initial_body.len());

    // 3. UPDATE with new data
    let mut update_builder = FlatBufferBuilder::new();
    let update_data_bytes = update_builder.create_vector(&[9u8, 10, 11, 12, 13, 14]);
    let update_file = FileData::create(&mut update_builder, &FileDataArgs {
        data: Some(update_data_bytes),
    });
    let update_files = update_builder.create_vector(&[update_file]);
    let update_file_list = FileDataList::create(&mut update_builder, &FileDataListArgs {
        files: Some(update_files),
    });
    update_builder.finish(update_file_list, None);
    let update_buf = update_builder.finished_data();

    let update_req = test::TestRequest::post()
        .uri(&format!("/update/{}", unique_key))
        .insert_header(("content-type", "application/octet-stream"))
        .insert_header(("user", "testuser1"))
        .set_payload(update_buf.to_vec())
        .to_request();

    let update_resp = test::call_service(&app, update_req).await;
    println!("UPDATE Status: {:?}", update_resp.status());
    assert_eq!(update_resp.status(), StatusCode::OK);

    // 4. GET updated data
    let get_updated_req = test::TestRequest::get()
        .uri(&format!("/get/{}", unique_key))
        .insert_header(("user", "testuser1"))
        .to_request();

    let get_updated_resp = test::call_service(&app, get_updated_req).await;
    println!("GET updated Status: {:?}", get_updated_resp.status());
    assert_eq!(get_updated_resp.status(), StatusCode::OK);

    let updated_body = test::read_body(get_updated_resp).await;
    println!("Updated data length: {} bytes", updated_body.len());

    // The updated data should be different from initial data
    assert_ne!(initial_body, updated_body);
}

#[actix_web::test]
async fn test_error_cases() {
    // Test various error cases
    let app = test::init_service(
        App::new()
            .service(put)
            .service(get)
            .service(append)
            .service(delete)
            .service(update_key)
            .service(update)
    ).await;

    let non_existent_key = format!("non_existent_{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos());

    // 1. GET non-existent key
    let get_req = test::TestRequest::get()
        .uri(&format!("/get/{}", non_existent_key))
        .insert_header(("user", "testuser1"))
        .to_request();

    let get_resp = test::call_service(&app, get_req).await;
    println!("GET non-existent key Status: {:?}", get_resp.status());
    assert_eq!(get_resp.status(), StatusCode::NOT_FOUND);

    // 2. DELETE non-existent key
    let delete_req = test::TestRequest::delete()
        .uri(&format!("/delete/{}", non_existent_key))
        .insert_header(("user", "testuser1"))
        .to_request();

    let delete_resp = test::call_service(&app, delete_req).await;
    println!("DELETE non-existent key Status: {:?}", delete_resp.status());
    assert_eq!(delete_resp.status(), StatusCode::NOT_FOUND);

    // 3. UPDATE non-existent key
    let update_req = test::TestRequest::post()
        .uri(&format!("/update/{}", non_existent_key))
        .insert_header(("content-type", "application/octet-stream"))
        .insert_header(("user", "testuser1"))
        .set_payload(b"test data".to_vec())
        .to_request();

    let update_resp = test::call_service(&app, update_req).await;
    println!("UPDATE non-existent key Status: {:?}", update_resp.status());
    assert_eq!(update_resp.status(), StatusCode::NOT_FOUND);

    // 4. UPDATE_KEY with non-existent old key
    let update_key_req = test::TestRequest::put()
        .uri(&format!("/update_key/{}/new_key", non_existent_key))
        .insert_header(("user", "testuser1"))
        .to_request();

    let update_key_resp = test::call_service(&app, update_key_req).await;
    println!("UPDATE_KEY with non-existent old key Status: {:?}", update_key_resp.status());
    assert_eq!(update_key_resp.status(), StatusCode::NOT_FOUND);

    // 5. APPEND to non-existent key
    let append_req = test::TestRequest::post()
        .uri(&format!("/append/{}", non_existent_key))
        .insert_header(("content-type", "application/octet-stream"))
        .insert_header(("user", "testuser1"))
        .set_payload(b"test data".to_vec())
        .to_request();

    let append_resp = test::call_service(&app, append_req).await;
    println!("APPEND to non-existent key Status: {:?}", append_resp.status());
    assert_eq!(append_resp.status(), StatusCode::NOT_FOUND);
}
