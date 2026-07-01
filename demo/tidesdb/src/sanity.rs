use tidesdb::{TidesDB, Config, ColumnFamilyConfig, LogLevel, ObjectStoreConfig, S3Config};
use std::process::Command;

const ENDPOINT: &str = "localhost:9710";
const BUCKET: &str = "tidesdb-sanity";
const ACCESS_KEY: &str = "adminkey";
const SECRET_KEY: &str = "adminsecretkey123456";

fn aws(args: &[&str]) -> std::process::Output {
    Command::new("aws")
        .args(args)
        .args(["--endpoint-url", &format!("http://{}", ENDPOINT)])
        .env("AWS_ACCESS_KEY_ID", ACCESS_KEY)
        .env("AWS_SECRET_ACCESS_KEY", SECRET_KEY)
        .env("AWS_DEFAULT_REGION", "us-east-1")
        .output()
        .expect("aws cli not found — install with: pip install awscli")
}

fn s3() -> S3Config {
    S3Config::new(ENDPOINT, BUCKET, ACCESS_KEY, SECRET_KEY)
        .region("us-east-1")
        .use_path_style(true)
        .use_ssl(false)
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let _ = std::fs::remove_dir_all("./sanity-local");
    aws(&["s3", "rb", &format!("s3://{}", BUCKET), "--force"]);
    let out = aws(&["s3api", "create-bucket", "--bucket", BUCKET]);
    println!("==> Bucket '{}' ready ({})", BUCKET, out.status);

    println!("\n==> Opening TidesDB (object store -> Warpdrive)...");
    let db = TidesDB::open(
        Config::new("./sanity-local")
            .object_store_s3(s3())
            .object_store_config(ObjectStoreConfig::new())
            .log_level(LogLevel::Info),
    )?;

    println!("\n==> Writing 100 KV pairs...");
    db.create_column_family("demo", ColumnFamilyConfig::default())?;
    let cf = db.get_column_family("demo")?;

    for i in 0u32..100 {
        let key = format!("key-{:04}", i).into_bytes();
        let val = format!("value-{:04} stored via TidesDB -> Warpdrive", i).into_bytes();
        let mut txn = db.begin_transaction()?;
        txn.put(&cf, &key, &val, -1)?;
        txn.commit()?;
    }
    println!("    Done.");

    println!("\n==> Flushing and compacting (uploads SSTables to Warpdrive)...");
    cf.flush_memtable()?;
    cf.compact()?;
    println!("    Done.");

    println!("\n==> Reading back sample keys:");
    for i in [0u32, 42, 99] {
        let key = format!("key-{:04}", i).into_bytes();
        let txn = db.begin_transaction()?;
        match txn.get(&cf, &key) {
            Ok(val) => println!("    {} = {}", String::from_utf8_lossy(&key), String::from_utf8_lossy(&val)),
            Err(e)  => println!("    {} = (err: {})", String::from_utf8_lossy(&key), e),
        }
    }

    println!("\n==> Objects stored in Warpdrive bucket '{}':", BUCKET);
    let out = aws(&["s3api", "list-objects-v2", "--bucket", BUCKET]);
    println!("{}", String::from_utf8_lossy(&out.stdout));

    let _ = std::fs::remove_dir_all("./sanity-local");
    Ok(())
}
