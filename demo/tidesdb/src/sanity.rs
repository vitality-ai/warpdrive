use tidesdb::{TidesDB, Config, ColumnFamilyConfig, LogLevel, ObjectStoreConfig};
use crate::{aws, s3_config};

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    println!("==> Preparing bucket in Warpdrive...");
    aws(&["s3", "rb", &format!("s3://{}", crate::BUCKET), "--force"]);
    let out = aws(&["s3api", "create-bucket", "--bucket", crate::BUCKET]);
    println!("    create-bucket: {}", out.status);

    println!("\n==> Opening TidesDB (object store -> Warpdrive)...");
    let db = TidesDB::open(
        Config::new("./tidesdb-primary")
            .object_store_s3(s3_config())
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

    println!("\n==> Objects stored in Warpdrive bucket '{}':", crate::BUCKET);
    let out = aws(&["s3api", "list-objects-v2", "--bucket", crate::BUCKET]);
    println!("{}", String::from_utf8_lossy(&out.stdout));

    Ok(())
}
