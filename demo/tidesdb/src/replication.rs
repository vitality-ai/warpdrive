use tidesdb::{TidesDB, Config, LogLevel, ObjectStoreConfig};
use crate::{aws, s3_config};
use std::thread;
use std::time::Duration;

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    // TidesDB replication is object-store-based: both primary and replica
    // point at the same Warpdrive bucket. No direct network link between
    // them — the bucket is the replication channel.

    // ---------------------------------------------------------------
    // Phase 1: primary — open against Warpdrive, write, flush, close
    // ---------------------------------------------------------------
    println!("--- Phase 1: Primary ---");
    println!("    Opening TidesDB as primary...");

    let primary = TidesDB::open(
        Config::new("./tidesdb-primary")
            .object_store_s3(s3_config())
            .object_store_config(
                ObjectStoreConfig::new()
                    .wal_sync_on_commit(true), // upload WAL after every commit so replica sees it fast
            )
            .log_level(LogLevel::Info),
    )?;

    // column family already exists from the sanity run; just get the handle
    let cf = primary.get_column_family("demo")?;

    for i in 100u32..150 {
        let key = format!("key-{:04}", i).into_bytes();
        let val = format!("value-{:04} written by primary", i).into_bytes();
        let mut txn = primary.begin_transaction()?;
        txn.put(&cf, &key, &val, -1)?;
        txn.commit()?;
    }
    println!("    Wrote 50 more keys.");

    cf.flush_memtable()?;
    drop(cf);
    drop(primary);
    println!("    Primary flushed and closed. Data is in Warpdrive.\n");

    // ---------------------------------------------------------------
    // Phase 2: replica — fresh local dir, same bucket, read-only
    // ---------------------------------------------------------------
    println!("--- Phase 2: Replica (read-only) ---");
    println!("    Opening replica against the same Warpdrive bucket...");

    let replica = TidesDB::open(
        Config::new("./tidesdb-replica")
            .object_store_s3(s3_config())
            .object_store_config(
                ObjectStoreConfig::new()
                    .replica_mode(true)
                    .replica_replay_wal(true)
                    .replica_sync_interval_us(200_000), // poll bucket every 200ms
            )
            .log_level(LogLevel::Info),
    )?;

    // Give the replica sync thread time to download SSTables from Warpdrive
    thread::sleep(Duration::from_millis(800));

    let rcf = replica.get_column_family("demo")?;

    let mut found = 0u32;
    for i in 0u32..150 {
        let key = format!("key-{:04}", i).into_bytes();
        let txn = replica.begin_transaction()?;
        if txn.get(&rcf, &key).is_ok() {
            found += 1;
        }
    }
    println!("    Read back {}/150 keys from Warpdrive via replica", found);

    let wtxn = replica.begin_transaction()?;
    match wtxn.put(&rcf, b"blocked", b"nope", -1) {
        Err(e) => println!("    Write rejected (correct): {}", e),
        Ok(_)  => println!("    Write unexpectedly succeeded!"),
    }
    println!();

    // ---------------------------------------------------------------
    // Phase 3: promote replica to primary
    // ---------------------------------------------------------------
    println!("--- Phase 3: Promote replica to primary ---");
    replica.promote_to_primary()?;
    println!("    Promoted.");

    let pcf = replica.get_column_family("demo")?;
    for i in 0u32..10 {
        let key = format!("promoted-key-{:04}", i).into_bytes();
        let val = b"written after promotion".to_vec();
        let mut txn = replica.begin_transaction()?;
        txn.put(&pcf, &key, &val, -1)?;
        txn.commit()?;
    }
    println!("    Wrote 10 new keys as promoted primary.");

    let txn = replica.begin_transaction()?;
    match txn.get(&pcf, b"promoted-key-0003") {
        Ok(val) => println!("    promoted-key-0003 = {}", String::from_utf8_lossy(&val)),
        Err(e)  => println!("    read err: {}", e),
    }

    println!("\n==> Warpdrive bucket '{}' served as the replication channel.", crate::BUCKET);

    // list final bucket state
    let out = aws(&["s3api", "list-objects-v2", "--bucket", crate::BUCKET]);
    println!("{}", String::from_utf8_lossy(&out.stdout));

    Ok(())
}
