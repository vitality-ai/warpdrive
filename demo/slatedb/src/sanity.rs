use object_store::aws::AmazonS3Builder;
use slatedb::Db;
use std::sync::Arc;

const ENDPOINT: &str = "http://localhost:9710";
const BUCKET: &str = "slatedb-sanity";
const ACCESS_KEY: &str = "adminkey";
const SECRET_KEY: &str = "adminsecretkey123456";
const LOCAL_CACHE: &str = "/tmp/slatedb-warpdrive-demo";

pub async fn run() -> anyhow::Result<()> {
    println!("== Sanity: basic KV operations over Warpdrive ==\n");

    let object_store = Arc::new(
        AmazonS3Builder::new()
            .with_allow_http(true)
            .with_endpoint(ENDPOINT)
            .with_access_key_id(ACCESS_KEY)
            .with_secret_access_key(SECRET_KEY)
            .with_bucket_name(BUCKET)
            .with_region("us-east-1")
            .build()?,
    );

    let db = Db::open(LOCAL_CACHE, object_store).await?;

    // Write
    println!("Writing 1000 keys...");
    for i in 0..1_000u32 {
        let key = format!("key-{i:04}");
        let val = format!("value-{i:04}");
        db.put(key.as_bytes(), val.as_bytes()).await?;
    }

    // Flush to Warpdrive (uploads SSTable files as S3 objects)
    println!("Flushing to Warpdrive...");
    db.flush().await?;

    // Read back a few
    println!("\nSpot-checking reads:");
    for i in [0u32, 42, 999] {
        let key = format!("key-{i:04}");
        let val = db.get(key.as_bytes()).await?;
        println!(
            "  {} => {}",
            key,
            val.map(|b| String::from_utf8_lossy(&b).to_string())
                .unwrap_or_else(|| "<missing>".into())
        );
    }

    // Scan a range
    println!("\nScanning key-0010..=key-0012:");
    let mut iter = db.scan("key-0010".as_bytes()..="key-0012".as_bytes()).await?;
    while let Ok(Some(kv)) = iter.next().await {
        println!(
            "  {} => {}",
            String::from_utf8_lossy(&kv.key),
            String::from_utf8_lossy(&kv.value)
        );
    }

    // Delete one key
    db.delete(b"key-0042").await?;
    db.flush().await?;
    let gone = db.get(b"key-0042").await?;
    println!("\nAfter deleting key-0042: {:?}", gone);

    db.close().await?;
    println!("\nDone.");

    Ok(())
}
